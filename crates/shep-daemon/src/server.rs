//! The connection layer: peer auth, handshake, subscriptions
//!
//! [`RpcServer`] owns the bound [`Listener`] and accepts connections until
//! told to stop. Each runs `handle_conn` in its own task: a same-uid check
//! ([`check_peer`], unix only), a version handshake, then a read loop that
//! decodes envelopes and hands them to
//! [`rpc::dispatch`](crate::rpc::dispatch), which never sees a socket.
//!
//! The OS transport lives in [`shep_core::transport`], so everything here is
//! one implementation over a unix socket and a Windows named pipe alike;
//! [`check_peer`] is the only genuine platform difference left.
//! [`RpcServer`]'s doc is the daemon's security writeup.

use core::fmt;
use core::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

use shep_core::protocol::{
    Envelope, Hello, HelloAck, HelloReply, PROTOCOL_VERSION, RpcError, RpcErrorCode, WireError,
    codec, decode_frame, encode_frame,
};
use shep_core::transport::{Listener, ServerReadHalf, ServerStream, ServerWriteHalf};

use crate::bus::spawn_forwarder;
use crate::rpc::{Outcome, RpcContext, dispatch};
use crate::supervisor::ConnId;

/// Frames queued toward one client before the connection back-pressures.
pub const CONN_QUEUE: usize = 64;
/// How long a connected peer has to send its `Hello` before the daemon closes.
pub const HANDSHAKE_TIMEOUT_MS: u64 = 5_000;

type Frames = FramedRead<ServerReadHalf, LengthDelimitedCodec>;

/// The control socket: shep's privilege boundary
///
/// # Security
///
/// The daemon's canonical writeup; other modules link here. On unix a
/// connection is refused unless `SO_PEERCRED`/`getpeereid` ([`check_peer`])
/// names the daemon's own uid, and refused too when the OS will not answer.
/// `$SHEP_HOME/run`'s `0700` is [`crate::boot::init_dirs`]'s job; Windows has
/// neither, and refuses at open time through the pipe's ACL. Skew, frame size,
/// a `Subscribe`'s glob count and a `/regex/` selector's compiled size are all
/// capped, and every call carries a clamped deadline. A same-uid peer is fully
/// trusted; there is no idle timeout and no per-uid connection cap.
#[derive(Debug)]
pub struct RpcServer {
    listener: Listener,
    ctx: RpcContext,
}

impl RpcServer {
    /// Wraps an already-bound listener with the request-handling context.
    #[must_use]
    pub fn new(listener: Listener, ctx: RpcContext) -> Self {
        Self { listener, ctx }
    }

    /// Accepts connections, each on its own task, until `shutdown` flips to
    /// `true` or its sender drops.
    ///
    /// Both `select!` branches are cancel-safe. A transient accept error such
    /// as `EMFILE` is logged and the loop continues.
    ///
    /// Connection tasks are spawned and detached, so `serve` returning does
    /// not mean every in-flight connection has finished. Draining them would
    /// need a `tokio::task::JoinSet` here.
    pub async fn serve(self, mut shutdown: watch::Receiver<bool>) {
        // `mut` because `Listener::accept` needs `&mut self` on both
        // platforms: a Windows named pipe server instance is consumed by
        // whoever connects to it, so accepting means handing that instance
        // out and creating the next one. See `shep_core::transport::Listener`.
        let Self { mut listener, ctx } = self;
        // A shutdown signal already `true` before the first `changed()` would
        // otherwise never be observed: `changed()` only resolves on a value
        // newer than the one this receiver has seen.
        if *shutdown.borrow() {
            return;
        }
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    match accepted {
                        Ok(stream) => {
                            let ctx = ctx.clone();
                            tokio::spawn(async move {
                                if let Err(err) = handle_conn(stream, ctx).await {
                                    tracing::debug!(%err, "connection ended");
                                }
                            });
                        }
                        Err(err) => tracing::warn!(%err, "accept failed; continuing"),
                    }
                }
                changed = shutdown.changed() => {
                    // An `Err` means the sender dropped: stop serving either
                    // way.
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    }
}

/// Checks that a connected peer runs as the daemon's own user.
///
// `UnixStream::peer_cred()` rather than nix's `PeerCredentials`, which nix
// gates behind `#[cfg(linux_android)]`, so it does not exist on macOS.
// tokio's `UCred` dispatches to `SO_PEERCRED`, `getpeereid` or
// `LOCAL_PEERCRED` per platform.
///
/// # Errors
/// - [`AuthError::NoCredentials`]: the OS would not report peer credentials.
/// - [`AuthError::ForeignUid`]: the peer's uid is not the daemon's.
///
/// # Platform
///
/// Unix only. The Windows pipe's ACL answers this question at open time; see
/// [`shep_core::transport`]'s module doc.
#[cfg(unix)]
pub fn check_peer(stream: &tokio::net::UnixStream, daemon_uid: u32) -> Result<u32, AuthError> {
    let cred = stream
        .peer_cred()
        .map_err(|err| AuthError::NoCredentials(err.to_string()))?;
    let peer = cred.uid();
    if peer == daemon_uid {
        Ok(peer)
    } else {
        Err(AuthError::ForeignUid {
            peer,
            daemon: daemon_uid,
        })
    }
}

/// The connecting peer's pid, when the OS will name one.
///
/// Separate from [`check_peer`], which reads the same
/// [`UCred`](tokio::net::unix::UCred): that answer admits or ends the
/// connection, and this is a diagnostic that must never do either.
///
/// `None` does not mean no process is there, only that the platform has no
/// answer; callers degrade through
/// [`Contact::Unknown`](crate::dogs::Contact::Unknown). Unix only.
#[cfg(unix)]
#[must_use]
pub fn peer_pid(stream: &tokio::net::UnixStream) -> Option<u32> {
    // Every failure is the same answer: an OS that would not say. No platform
    // here produces a pid too wide for `u32`.
    u32::try_from(stream.peer_cred().ok()?.pid()?).ok()
}

/// The daemon's effective uid.
///
/// # Platform
///
/// Unix only, alongside [`check_peer`], its only caller.
#[cfg(unix)]
#[must_use]
pub fn daemon_uid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

/// Why [`check_peer`] refused a connection.
///
/// `#[non_exhaustive]`: a future check, a group membership or a peer
/// certificate, would need its own variant rather than stretching
/// [`Self::ForeignUid`] to mean something it does not.
#[non_exhaustive]
#[cfg_attr(windows, allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// The OS would not report peer credentials on this socket (carries the
    /// OS error message).
    NoCredentials(String),
    /// The peer runs as another user (carries both uids).
    ForeignUid {
        /// The connecting peer's uid.
        peer: u32,
        /// The daemon's own uid.
        daemon: u32,
    },
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCredentials(msg) => write!(f, "could not read peer credentials: {msg}"),
            Self::ForeignUid { peer, daemon } => {
                write!(f, "peer uid {peer} does not match daemon uid {daemon}")
            }
        }
    }
}

impl core::error::Error for AuthError {}

/// Error type ending one connection.
///
/// Every variant is terminal: the connection layer logs it and closes the
/// socket. A malformed or hostile peer can only cost itself its connection.
///
/// `#[non_exhaustive]`: a future failure point, a TLS handshake or a
/// rate-limit refusal, would add its own variant rather than overloading
/// [`Self::Auth`], which is specifically [`check_peer`]'s verdict.
#[non_exhaustive]
#[derive(Debug)]
pub enum ConnError {
    /// [`check_peer`] refused the connection.
    Auth(AuthError),
    /// The framed transport failed reading or writing a length-delimited frame.
    Frame(std::io::Error),
    /// A frame's payload failed to decode as the expected type.
    Decode(WireError),
    /// A reply or event failed to encode onto the wire.
    Encode(WireError),
    /// The peer's `Hello.protocol` did not match [`PROTOCOL_VERSION`] (carries
    /// the client's claimed version; the refusal is written before this is
    /// returned).
    ProtocolMismatch {
        /// The protocol version the client sent.
        client: u32,
    },
    /// The peer did not send `Hello` within [`HANDSHAKE_TIMEOUT_MS`].
    HandshakeTimeout,
    /// The peer closed the connection before sending `Hello`.
    NoHandshake,
    /// The connection's write queue is gone: the writer task exited.
    PeerGone,
}

impl fmt::Display for ConnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auth(err) => write!(f, "peer-credential check failed: {err}"),
            Self::Frame(err) => write!(f, "frame transport error: {err}"),
            Self::Decode(err) => write!(f, "frame decode error: {err}"),
            Self::Encode(err) => write!(f, "frame encode error: {err}"),
            Self::ProtocolMismatch { client } => write!(
                f,
                "client sent protocol {client}, daemon speaks {PROTOCOL_VERSION}"
            ),
            Self::HandshakeTimeout => write!(
                f,
                "peer did not send Hello within {HANDSHAKE_TIMEOUT_MS} ms"
            ),
            Self::NoHandshake => f.write_str("peer closed before sending Hello"),
            Self::PeerGone => f.write_str("connection's write queue is gone"),
        }
    }
}

impl core::error::Error for ConnError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Auth(err) => Some(err),
            Self::Frame(err) => Some(err),
            Self::Decode(err) | Self::Encode(err) => Some(err),
            Self::ProtocolMismatch { .. }
            | Self::HandshakeTimeout
            | Self::NoHandshake
            | Self::PeerGone => None,
        }
    }
}

impl From<AuthError> for ConnError {
    fn from(source: AuthError) -> Self {
        Self::Auth(source)
    }
}

// `Decode` and `Encode` both wrap `WireError`, so only one could claim
// `impl From<WireError> for ConnError` and a bare `?` would silently mislabel
// the other direction. Both stay explicit `map_err` calls.

impl From<std::io::Error> for ConnError {
    fn from(source: std::io::Error) -> Self {
        Self::Frame(source)
    }
}

// The ordering here is load-bearing (see `handshake` and `converse` below):
// auth before a single byte is read from the peer, the handshake before any
// request, and the writer task joined on every exit path so a protocol-skew
// refusal is guaranteed to reach the wire before the socket closes.
async fn handle_conn(stream: ServerStream, ctx: RpcContext) -> Result<(), ConnError> {
    // Unix only. On Windows the pipe's own ACL refuses a foreign user's
    // open-for-write before a byte reaches this function, so the equivalent
    // check has already happened in the kernel.
    #[cfg(unix)]
    check_peer(&stream, daemon_uid())?;
    // Read once here and carried down to the handshake: this is what lets the
    // silence ladder tell a dog that never reached the socket from one that
    // reached it and would not say who it was. `None` on Windows.
    #[cfg(unix)]
    let peer = peer_pid(&stream);
    #[cfg(not(unix))]
    let peer: Option<u32> = None;
    // Recorded before a byte is read: a peer that connects and then says
    // nothing at all has still reached this daemon.
    if let Some(pid) = peer {
        ctx.peer_contacts.connected(pid);
    }
    // Minted after the peer check, not before: a connection refused for its
    // uid never reaches a handler, so it has nothing to scope.
    let conn = ConnId::next();
    let (read_half, write_half) = shep_core::transport::split(stream);
    let mut frames = FramedRead::new(read_half, codec());
    let (out_tx, out_rx) = mpsc::channel::<Bytes>(CONN_QUEUE);
    let writer = tokio::spawn(write_loop(FramedWrite::new(write_half, codec()), out_rx));

    let outcome = converse(&mut frames, &out_tx, conn, peer, &ctx).await;

    // Drop the sender and join the writer on every path: a protocol-skew
    // refusal is written by that task, so returning early would close the
    // socket before the client saw why.
    drop(out_tx);
    let _ = writer.await;
    // On every path out, for the same reason. A smit belongs to the connection
    // that painted it. After the writer join, so a client that painted and
    // immediately read still sees its own mark in the reply.
    ctx.supervisor.forget_smits(conn).await;
    outcome
}

async fn converse(
    frames: &mut Frames,
    out: &mpsc::Sender<Bytes>,
    conn: ConnId,
    peer: Option<u32>,
    ctx: &RpcContext,
) -> Result<(), ConnError> {
    handshake(frames, out, peer, ctx).await?;
    let mut forwarder: Option<JoinHandle<()>> = None;
    let outcome = read_loop(frames, out, conn, ctx, &mut forwarder).await;
    // Every path out of `read_loop` lands here, and a live forwarder must be
    // aborted rather than dropped: dropping a `JoinHandle` detaches the task,
    // which keeps its clone of `out` alive, which keeps `write_loop` from ever
    // seeing every sender gone and hangs `handle_conn`'s `writer.await`.
    if let Some(forwarder) = forwarder {
        forwarder.abort();
    }
    outcome
}

async fn read_loop(
    frames: &mut Frames,
    out: &mpsc::Sender<Bytes>,
    conn: ConnId,
    ctx: &RpcContext,
    forwarder: &mut Option<JoinHandle<()>>,
) -> Result<(), ConnError> {
    while let Some(frame) = frames.next().await {
        let frame = frame?; // oversize/short frame ends the connection
        let envelope: Envelope = decode_frame(&frame).map_err(ConnError::Decode)?;
        match dispatch(envelope, conn, ctx).await {
            Outcome::Reply(reply) => send(out, &reply).await?,
            Outcome::Subscribe { reply, filter } => {
                send(out, &reply).await?; // ordered ahead of any event by the queue
                // A second Subscribe REPLACES the first: spec §6 gives a
                // connection one topic list, not a growing union.
                if let Some(old) =
                    forwarder.replace(spawn_forwarder(&ctx.events, filter, out.clone()))
                {
                    old.abort();
                }
            }
            Outcome::Shutdown(reply) => {
                send(out, &reply).await?;
                ctx.shutdown();
                break;
            }
        }
    }
    Ok(())
}

async fn handshake(
    frames: &mut Frames,
    out: &mpsc::Sender<Bytes>,
    peer: Option<u32>,
    ctx: &RpcContext,
) -> Result<(), ConnError> {
    let frame = tokio::time::timeout(Duration::from_millis(HANDSHAKE_TIMEOUT_MS), frames.next())
        .await
        .map_err(|_| ConnError::HandshakeTimeout)?
        .ok_or(ConnError::NoHandshake)??;
    let hello: Hello = decode_frame(&frame).map_err(ConnError::Decode)?;
    // Ahead of the protocol check: this records what the peer sent, not what
    // this daemon made of it. A dog refused on skew still named itself.
    if hello.dog_name.is_some()
        && let Some(pid) = peer
    {
        ctx.peer_contacts.named_a_dog(pid);
    }
    if hello.protocol != PROTOCOL_VERSION {
        // Version skew is a typed error, not silence (spec §6).
        let refusal: HelloReply = Err(RpcError {
            code: RpcErrorCode::ProtocolMismatch,
            message: format!(
                "daemon speaks protocol {PROTOCOL_VERSION}, client sent {}",
                hello.protocol
            ),
            // The refusal names our protocol, which does not say which shep
            // is running. `shep daemon reload` chooses its mechanism by
            // version, and this is the one path where the ack never arrives.
            daemon_version: Some(ctx.daemon_version.clone()),
        });
        send(out, &refusal).await?;
        // The refusal is queued before anything else happens, so a peer about
        // to be restarted still learns why. `dog_name` is `None` for every
        // client that is not a dog: the name travels only on
        // `ReconnectingClient`, which no `shep` verb uses.
        match &hello.dog_name {
            Some(dog) => {
                crate::dogs::record_refused_dog(
                    dog,
                    &hello.client_version,
                    &ctx.dog_refusals,
                    &ctx.supervisor,
                );
                // Into the dog's own log too, carrying both protocol numbers:
                // that log is where an operator looks first.
                crate::dogs::narrate_by_name(
                    &ctx.supervisor,
                    &ctx.events,
                    dog,
                    format!(
                        "shep REFUSED this dog's handshake: this shepherd speaks protocol {PROTOCOL_VERSION} and the dog sent {}. Its own build is shep-client {}. Rebuild or reinstall it against this shep and run `shep restart {dog}`",
                        hello.protocol, hello.client_version
                    ),
                );
            }
            // An operator running an older `shep` already reads the skew from
            // their own CLI. `debug!`, not `warn!`: the CLI polls while it
            // waits for a successor, so one reload across a protocol bump
            // produced 442 of these in 9.8 seconds.
            None => tracing::debug!(
                client_protocol = hello.protocol,
                client_version = %hello.client_version,
                "refused a client on protocol skew"
            ),
        }
        return Err(ConnError::ProtocolMismatch {
            client: hello.protocol,
        });
    }
    // A dog that got in is not stale, including after the restart it was just
    // given, which is the case that has to clear.
    if let Some(dog) = &hello.dog_name {
        // Only the transition is narrated: a dog reconnects after a handover
        // or a daemon restart, and a line per connection would bury its own
        // output in its own log.
        if ctx.dog_refusals.handshook(dog) {
            crate::dogs::narrate_by_name(
                &ctx.supervisor,
                &ctx.events,
                dog,
                format!(
                    "shep accepted this dog's handshake; it is registered with this shepherd as `{dog}`, on protocol {PROTOCOL_VERSION}"
                ),
            );
        }
    }
    let ack: HelloReply = Ok(HelloAck {
        daemon_version: ctx.daemon_version.clone(),
        protocol: PROTOCOL_VERSION,
        pid: ctx.pid,
    });
    send(out, &ack).await
}

async fn write_loop(
    mut sink: FramedWrite<ServerWriteHalf, LengthDelimitedCodec>,
    mut rx: mpsc::Receiver<Bytes>,
) {
    while let Some(bytes) = rx.recv().await {
        if sink.send(bytes).await.is_err() {
            break; // peer gone; nothing left to drain the queue
        }
    }
}

async fn send<T: Serialize>(out: &mpsc::Sender<Bytes>, value: &T) -> Result<(), ConnError> {
    let bytes = encode_frame(value).map_err(ConnError::Encode)?;
    out.send(bytes).await.map_err(|_| ConnError::PeerGone)
}

#[cfg(test)]
mod tests {
    // Real time: every test here drives a real socket, and a paused clock
    // auto-advances when the runtime idles, expiring HANDSHAKE_TIMEOUT_MS
    // before the peer's bytes arrive.
    use super::*;
    use crate::bus::SharedEvent;
    use crate::fake::{FIRST_SCRIPTED_PID, ProcScript};
    use crate::testing::harness;
    use futures_util::{SinkExt, StreamExt};
    use serde::Serialize;
    use serde::de::DeserializeOwned;
    use shep_core::protocol::{
        BusEvent, DogSource, Envelope, Hello, HelloReply, PROTOCOL_VERSION, ProcessEventKind,
        ProcessInfo, Request, Response, RpcErrorCode, ServerFrame, codec, decode_frame,
        encode_frame,
    };
    use shep_core::status::ProcStatus;
    use tokio_util::codec::Framed;

    const RECV_TIMEOUT: Duration = Duration::from_secs(5);

    struct Client {
        frames: Framed<shep_core::transport::ClientStream, tokio_util::codec::LengthDelimitedCodec>,
    }

    impl Client {
        async fn send<T: Serialize>(&mut self, value: &T) {
            self.frames
                .send(encode_frame(value).unwrap())
                .await
                .unwrap();
        }

        async fn recv<T: DeserializeOwned>(&mut self) -> T {
            let frame = tokio::time::timeout(RECV_TIMEOUT, self.frames.next())
                .await
                .expect("timed out waiting for a frame")
                .expect("connection closed early")
                .unwrap();
            decode_frame(&frame).unwrap()
        }

        async fn closed(&mut self) -> bool {
            tokio::time::timeout(RECV_TIMEOUT, self.frames.next())
                .await
                .expect("timed out waiting for close")
                .is_none()
        }
    }

    /// Spawns `handle_conn` over a real connected pair and hands back the
    /// client end.
    ///
    /// A real transport on both platforms, a socketpair on unix and a named
    /// pipe on Windows, rather than an in-memory duplex: several tests below
    /// turn on what a peer sees when the other side closes.
    async fn connected(ctx: RpcContext) -> Client {
        let (server, client) = shep_core::transport::connected_pair().await.unwrap();
        tokio::spawn(async move {
            let _ = handle_conn(server, ctx).await;
        });
        Client {
            frames: Framed::new(client, codec()),
        }
    }

    #[tokio::test]
    async fn handshake_acks_a_matching_protocol() {
        let h = harness(vec![]);
        let mut client = connected(h.ctx.clone()).await;
        client
            .send(&Hello {
                client_version: "0.1.0".to_string(),
                protocol: PROTOCOL_VERSION,
                dog_name: None,
            })
            .await;
        let ack: HelloReply = client.recv().await;
        let ack = ack.expect("a matching protocol must be acked");
        assert_eq!(ack.protocol, PROTOCOL_VERSION);
        assert_eq!(ack.pid, h.ctx.pid);
        assert_eq!(ack.daemon_version, h.ctx.daemon_version);
    }

    #[tokio::test]
    async fn handshake_refuses_protocol_skew_before_closing() {
        let h = harness(vec![]);
        let mut client = connected(h.ctx.clone()).await;
        client
            .send(&Hello {
                client_version: "9.9.9".to_string(),
                protocol: PROTOCOL_VERSION + 1,
                dog_name: None,
            })
            .await;
        let refusal: HelloReply = client.recv().await;
        let err = refusal.expect_err("skew must be refused");
        assert_eq!(err.code, RpcErrorCode::ProtocolMismatch);
        assert!(
            client.closed().await,
            "the daemon must close after refusing"
        );
    }

    #[tokio::test]
    async fn a_protocol_refusal_carries_the_daemon_version() {
        // `shep daemon reload` picks between a handover and a stop-and-start
        // by crate version, which the protocol number does not give it.
        let h = harness(vec![]);
        let mut client = connected(h.ctx.clone()).await;
        client
            .send(&Hello {
                client_version: "9.9.9".to_string(),
                protocol: PROTOCOL_VERSION + 1,
                dog_name: None,
            })
            .await;
        let refusal: HelloReply = client.recv().await;
        let err = refusal.expect_err("skew must be refused");
        assert_eq!(err.code, RpcErrorCode::ProtocolMismatch);
        // The same field the ack uses: a client must never learn two versions
        // for one daemon.
        assert_eq!(err.daemon_version.as_deref(), Some(&*h.ctx.daemon_version));
    }

    #[tokio::test]
    async fn a_request_before_the_handshake_ends_the_connection() {
        let h = harness(vec![]);
        let mut client = connected(h.ctx.clone()).await;
        client
            .send(&Envelope {
                id: 1,
                deadline_ms: None,
                body: Request::Ping,
            })
            .await;
        assert!(client.closed().await);
    }

    #[tokio::test]
    async fn ping_round_trips_over_the_socket() {
        let h = harness(vec![]);
        let mut client = connected(h.ctx.clone()).await;
        client
            .send(&Hello {
                client_version: "0.1.0".to_string(),
                protocol: PROTOCOL_VERSION,
                dog_name: None,
            })
            .await;
        let _: HelloReply = client.recv().await;
        client
            .send(&Envelope {
                id: 11,
                deadline_ms: Some(1000),
                body: Request::Ping,
            })
            .await;
        let frame: ServerFrame = client.recv().await;
        let ServerFrame::Reply(reply) = frame else {
            panic!("expected a reply frame")
        };
        assert_eq!(reply.id, 11);
        assert_eq!(reply.result.unwrap(), Response::Pong);
    }

    #[tokio::test]
    async fn subscribe_streams_only_matching_events() {
        let h = harness(vec![]);
        let mut client = connected(h.ctx.clone()).await;
        client
            .send(&Hello {
                client_version: "0.1.0".to_string(),
                protocol: PROTOCOL_VERSION,
                dog_name: None,
            })
            .await;
        let _: HelloReply = client.recv().await;
        client
            .send(&Envelope {
                id: 1,
                deadline_ms: None,
                body: Request::Subscribe {
                    topics: vec!["process.*".to_string()],
                },
            })
            .await;
        let frame: ServerFrame = client.recv().await;
        assert!(matches!(frame, ServerFrame::Reply(ref r) if r.result == Ok(Response::Subscribed)));

        let event = |kind| -> SharedEvent {
            BusEvent::Process {
                event: kind,
                info: ProcessInfo::builder(0, "web", shep_core::status::ProcStatus::Online)
                    .pid(Some(1000))
                    .out_file(Some("/logs/web-0-out.log".to_string()))
                    .err_file(Some("/logs/web-0-err.log".to_string()))
                    .build(),
                manually: false,
                at_ms: 0,
            }
            .into()
        };
        h.ctx.events.send(event(ProcessEventKind::Start)).unwrap();
        h.ctx
            .events
            .send(
                BusEvent::LogOut {
                    id: 0,
                    line: "filtered".to_string(),
                }
                .into(),
            )
            .unwrap();
        h.ctx.events.send(event(ProcessEventKind::Online)).unwrap();

        // Back-to-back arrival is the filtering assertion: no negative wait.
        let first: ServerFrame = client.recv().await;
        let second: ServerFrame = client.recv().await;
        assert!(matches!(
            first,
            ServerFrame::Event(BusEvent::Process {
                event: ProcessEventKind::Start,
                ..
            })
        ));
        assert!(matches!(
            second,
            ServerFrame::Event(BusEvent::Process {
                event: ProcessEventKind::Online,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_garbage_frame_ends_the_connection_without_panicking() {
        let h = harness(vec![]);
        let mut client = connected(h.ctx.clone()).await;
        client
            .send(&Hello {
                client_version: "0.1.0".to_string(),
                protocol: PROTOCOL_VERSION,
                dog_name: None,
            })
            .await;
        let _: HelloReply = client.recv().await;
        client
            .frames
            .send(bytes::Bytes::from_static(b"not json"))
            .await
            .unwrap();
        assert!(client.closed().await);
    }

    #[tokio::test]
    async fn a_garbage_frame_after_subscribing_still_closes_the_connection() {
        // A live forwarder holds its own clone of `out`. Not aborting it on a
        // connection error leaves `out_tx`'s drop short of the last sender, so
        // `writer.await` hangs and the socket never closes. Subscribing first
        // is what makes that path reachable.
        let h = harness(vec![]);
        let mut client = connected(h.ctx.clone()).await;
        client
            .send(&Hello {
                client_version: "0.1.0".to_string(),
                protocol: PROTOCOL_VERSION,
                dog_name: None,
            })
            .await;
        let _: HelloReply = client.recv().await;
        client
            .send(&Envelope {
                id: 1,
                deadline_ms: None,
                body: Request::Subscribe {
                    topics: vec!["*".to_string()],
                },
            })
            .await;
        let _: ServerFrame = client.recv().await; // the Subscribed reply
        client
            .frames
            .send(bytes::Bytes::from_static(b"not json"))
            .await
            .unwrap();
        assert!(
            client.closed().await,
            "a live forwarder must not keep the connection open past a decode error"
        );
    }

    /// `cfg(unix)`, like [`check_peer`] itself: the Windows pipe's ACL
    /// refuses a foreign user before `handle_conn` is reached at all.
    #[cfg(unix)]
    #[tokio::test]
    async fn peer_credentials_gate_on_uid() {
        // `UnixStream::pair()` reports both ends as this process's own uid,
        // and `UCred` has no synthetic constructor, so only the `daemon_uid`
        // argument can vary: this pins the comparison, not a real cross-uid
        // connection.
        let (a, _b) = tokio::net::UnixStream::pair().unwrap();
        let me = daemon_uid();
        assert_eq!(check_peer(&a, me).unwrap(), me);
        assert_eq!(
            check_peer(&a, me + 1).unwrap_err(),
            AuthError::ForeignUid {
                peer: me,
                daemon: me + 1
            }
        );
    }

    #[tokio::test]
    async fn a_slow_subscriber_gets_a_dropped_notice_instead_of_hanging_the_bus() {
        // Drives the Lagged-to-Dropped translation through the real
        // connection stack rather than in isolation. Real time: real socket.
        let h = harness(vec![]);
        let mut client = connected(h.ctx.clone()).await;
        client
            .send(&Hello {
                client_version: "0.1.0".to_string(),
                protocol: PROTOCOL_VERSION,
                dog_name: None,
            })
            .await;
        let _: HelloReply = client.recv().await;
        client
            .send(&Envelope {
                id: 1,
                deadline_ms: None,
                body: Request::Subscribe {
                    topics: vec!["log.*".to_string()],
                },
            })
            .await;
        let _: ServerFrame = client.recv().await; // the Subscribed reply

        // Never call `client.recv()` again until after the flood: CONN_QUEUE
        // fills, the forwarder blocks on `out.send`, and the broadcast ring
        // takes the overflow from there.
        let flood = crate::bus::BUS_CAPACITY + CONN_QUEUE + 16;
        for i in 0..flood {
            h.ctx
                .events
                .send(
                    BusEvent::LogOut {
                        id: 0,
                        line: format!("line-{i}"),
                    }
                    .into(),
                )
                .unwrap();
        }

        // Resume reading. The count comes from tokio's own `Lagged(n)` inside
        // the forwarder, never hand-computed here.
        let dropped = loop {
            match client.recv::<ServerFrame>().await {
                ServerFrame::Event(BusEvent::Dropped { count }) => break count,
                ServerFrame::Event(_) => continue,
                other => panic!("expected eventually a Dropped notice, got {other:?}"),
            }
        };
        assert!(
            dropped > 0,
            "a flood past CONN_QUEUE + BUS_CAPACITY must report a real lag"
        );
    }

    // --- G8: what a refused DOG's handshake costs it ------------------
    //
    // A refused handshake never reaches a request, so `Hello.dog_name` is the
    // only place the daemon learns which dog it just refused.

    /// Registers `name` as a built-in dog and returns the row it produced.
    ///
    /// Straight through [`crate::supervisor::SupervisorHandle::start_dog`]:
    /// `Request::EnableDog` would need a handshaken connection of its own.
    async fn start_dog(ctx: &RpcContext, name: &str) -> ProcessInfo {
        let spec = crate::dogs::DogSpec {
            name: name.to_string(),
            source: DogSource::BuiltIn,
        };
        let app = crate::dogs::dog_app(&spec, &ctx.paths).expect("the dog fixture must assemble");
        ctx.supervisor
            .start_dog(app, DogSource::BuiltIn)
            .await
            .expect("the dog fixture must start")
    }

    /// One refused handshake, announcing `dog` (or nothing, for a client
    /// that is not a dog), returning once the daemon has closed on it.
    ///
    /// The daemon records the refusal and decides what it owes the dog before
    /// it returns the error that closes the socket, so a caller that has seen
    /// the close can read the verdict without racing it. The restart itself
    /// runs on its own task and needs [`await_dog`].
    async fn refuse_as(ctx: &RpcContext, dog: Option<&str>) {
        let mut client = connected(ctx.clone()).await;
        client
            .send(&Hello {
                client_version: "0.1.14".to_string(),
                protocol: PROTOCOL_VERSION + 1,
                dog_name: dog.map(str::to_owned),
            })
            .await;
        let refusal: HelloReply = client.recv().await;
        refusal.expect_err("a skewed protocol must be refused");
        assert!(
            client.closed().await,
            "the daemon must close after refusing"
        );
    }

    /// The flock row named `name`, or a panic naming what was there.
    async fn dog_row(ctx: &RpcContext, name: &str) -> ProcessInfo {
        ctx.supervisor
            .list()
            .await
            .into_iter()
            .find(|info| info.name == name)
            .unwrap_or_else(|| panic!("no row named {name}"))
    }

    /// Waits until `name` is running as `pid`, or fails inside
    /// [`RECV_TIMEOUT`], returning how long it took.
    ///
    /// The restart a refusal triggers runs on its own task, so there is no
    /// handle to await. The elapsed time is returned because the
    /// never-restart-twice test below sizes its negative window against it.
    async fn await_dog(ctx: &RpcContext, name: &str, pid: u32) -> Duration {
        let began = tokio::time::Instant::now();
        let seen = tokio::time::timeout(RECV_TIMEOUT, async {
            loop {
                if dog_row(ctx, name).await.pid == Some(pid) {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            seen.is_ok(),
            "{name} never reached pid {pid} within {RECV_TIMEOUT:?}: {:?}",
            ctx.supervisor.list().await
        );
        began.elapsed()
    }

    /// fails if a refused dog is left mute. The daemon is the only party that
    /// can restart it: the dog's own client has stopped rather than spinning.
    ///
    /// The pid moving is the assertion, not the restart count: a restart that
    /// re-registered the row without re-spawning leaves the dog as mute.
    #[tokio::test]
    async fn a_refused_dog_is_restarted_once_from_the_binary_on_disk() {
        let h = harness(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        let started = start_dog(&h.ctx, "metrics").await;
        assert_eq!(started.pid, Some(FIRST_SCRIPTED_PID));

        refuse_as(&h.ctx, Some("metrics")).await;

        await_dog(&h.ctx, "metrics", FIRST_SCRIPTED_PID + 1).await;
        assert!(
            h.ctx.dog_refusals.stale().is_empty(),
            "one refusal buys a restart; it does not condemn the dog"
        );
    }

    /// fails if the daemon restarts a dog it has already restarted, the spin
    /// G8 forbids. A second refusal proves the binary on disk cannot satisfy
    /// this daemon either, since the restart already ran it.
    ///
    /// The pid must not move inside a window sized against the restart that
    /// really happened earlier in this test, and the harness is scripted with
    /// exactly the two spawns G8 permits.
    #[tokio::test]
    async fn a_twice_refused_dog_is_reported_stale_and_never_restarted_again() {
        let h = harness(vec![ProcScript::never_exits(), ProcScript::never_exits()]);
        start_dog(&h.ctx, "metrics").await;

        refuse_as(&h.ctx, Some("metrics")).await;
        let restart_took = await_dog(&h.ctx, "metrics", FIRST_SCRIPTED_PID + 1).await;

        refuse_as(&h.ctx, Some("metrics")).await;
        assert_eq!(
            h.ctx.dog_refusals.stale(),
            vec!["metrics".to_string()],
            "the second refusal must be reported, not swallowed"
        );

        // Ten times the restart that did happen, floored so a fast machine
        // still watches for a real interval.
        let window = (restart_took * 10).max(Duration::from_millis(200));
        tokio::time::sleep(window).await;

        let after = dog_row(&h.ctx, "metrics").await;
        assert_eq!(
            after.pid,
            Some(FIRST_SCRIPTED_PID + 1),
            "a second restart within {window:?} is the spin G8 forbids"
        );
        assert_eq!(
            after.status,
            ProcStatus::Online,
            "a third spawn would exhaust the script and error the dog"
        );
    }

    /// fails if a dog that got back in stays condemned. Without the mark
    /// clearing, the dog is reported stale forever while answering perfectly.
    #[tokio::test]
    async fn a_dog_that_handshakes_is_no_longer_stale() {
        let h = harness(vec![]);
        refuse_as(&h.ctx, Some("metrics")).await;
        refuse_as(&h.ctx, Some("metrics")).await;
        assert_eq!(h.ctx.dog_refusals.stale(), vec!["metrics".to_string()]);

        let mut client = connected(h.ctx.clone()).await;
        client
            .send(&Hello {
                client_version: "0.1.22".to_string(),
                protocol: PROTOCOL_VERSION,
                dog_name: Some("metrics".to_string()),
            })
            .await;
        let ack: HelloReply = client.recv().await;
        ack.expect("a matching protocol must be acked");

        assert!(
            h.ctx.dog_refusals.stale().is_empty(),
            "a dog talking to this daemon is not stale by any definition it can apply"
        );
    }

    /// fails if an operator running an older `shep` has a dog restarted under
    /// them. The CLI cannot name a dog, so a refusal carrying no name must
    /// leave the flock exactly as it was.
    #[tokio::test]
    async fn a_refused_client_that_is_not_a_dog_touches_nothing() {
        let h = harness(vec![ProcScript::never_exits()]);
        let started = start_dog(&h.ctx, "metrics").await;

        for _ in 0..3 {
            refuse_as(&h.ctx, None).await;
        }

        assert!(h.ctx.dog_refusals.stale().is_empty());
        let after = dog_row(&h.ctx, "metrics").await;
        assert_eq!(
            after.pid, started.pid,
            "a nameless refusal must not restart anything"
        );
        assert_eq!(after.status, ProcStatus::Online);
    }

    // --- The incident: a dog that reaches shep and never names itself ---

    /// fails if a dog that is CONNECTED to this shepherd is reported as a
    /// binary that cannot talk to it.
    ///
    /// End to end rather than against `DogRefusals`: the ladder's verdict was
    /// already right and only the sentence drawn from it was wrong.
    /// `harness_at_pid` runs the scripted dog at this test's own pid, so the
    /// peer credentials the daemon reads name one process. Unix only:
    /// `peer_pid` answers `None` on Windows and the ladder reaches
    /// [`Silence::Unattributed`](crate::dogs::Silence::Unattributed) instead.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_dog_that_connects_without_naming_itself_is_not_called_a_stale_binary() {
        // Three: the first spawn, the restart the ladder's first rung asks
        // for, and one spare so a spawn is never refused for want of script.
        let h = crate::testing::harness_at_pid(
            vec![
                ProcScript::never_exits(),
                ProcScript::never_exits(),
                ProcScript::never_exits(),
            ],
            std::process::id(),
        );
        // A real socket cannot pause its clock, so the attribution warm-up is
        // forced rather than walked;
        // `dogs::a_dog_that_never_calls_still_earns_its_rebuild_after_the_warm_up`
        // covers the boundary.
        h.ctx.peer_contacts.force_warm();
        let dog = start_dog(&h.ctx, "log-rotate").await;
        assert_eq!(
            dog.pid,
            Some(std::process::id()),
            "the fixture only means anything if the dog and this test are one process"
        );
        let err_log = dog
            .err_file
            .clone()
            .expect("a dog's listing resolves its log paths");

        // The current protocol, a real client version, and no `dog_name`.
        let mut client = connected(h.ctx.clone()).await;
        client
            .send(&Hello {
                protocol: PROTOCOL_VERSION,
                client_version: "0.1.22".to_string(),
                dog_name: None,
            })
            .await;
        let ack: HelloReply = client.recv().await;
        ack.expect("an anonymous handshake on the current protocol is ACCEPTED, not refused");

        // And it serves requests, which is the half the verdict has to admit.
        client
            .send(&Envelope {
                id: 1,
                deadline_ms: None,
                body: Request::ListFlock,
            })
            .await;
        match client.recv::<ServerFrame>().await {
            ServerFrame::Reply(reply) => match reply.result {
                Ok(Response::Flock(flock)) => assert!(
                    flock.iter().any(|info| info.name == "log-rotate"),
                    "the connection this daemon is about to call stale is serving requests"
                ),
                other => panic!("ListFlock must answer with the flock, got {other:?}"),
            },
            other => panic!("an accepted connection must serve ListFlock, got {other:?}"),
        }

        assert_eq!(
            h.ctx.peer_contacts.from_pid(Some(std::process::id())),
            crate::dogs::Contact::Anonymous,
            "the handshake path must record what actually arrived"
        );
        assert!(
            !h.ctx.dog_refusals.has_handshook("log-rotate"),
            "no `dog_name` means no handshake was recorded, which is the whole trap"
        );

        // The real ladder over two whole budgets. Instants rather than a
        // paused clock: the connection above is a real socket, and a paused
        // runtime auto-advances whenever it idles.
        let mut seen = crate::dogs::SilentDogs::default();
        let t0 = tokio::time::Instant::now();
        let ladder = async |seen: &mut crate::dogs::SilentDogs, at| {
            crate::dogs::check_silent_dogs(
                &h.ctx.supervisor,
                &h.ctx.dog_refusals,
                &h.ctx.peer_contacts,
                &h.ctx.events,
                seen,
                at,
            )
            .await
        };
        assert!(ladder(&mut seen, t0).await.is_empty());
        assert_eq!(
            ladder(&mut seen, t0 + crate::dogs::DOG_SILENCE_BUDGET).await,
            vec![("log-rotate".to_string(), crate::dogs::Refusal::Restart)]
        );
        assert_eq!(
            ladder(&mut seen, t0 + 2 * crate::dogs::DOG_SILENCE_BUDGET).await,
            vec![("log-rotate".to_string(), crate::dogs::Refusal::Stale)],
            "the ladder's verdict is unchanged; what changes is what it SAYS"
        );

        // What the operator reads: the dog's own log, the file the verdict
        // tells them to open.
        let written = std::fs::read_to_string(&err_log).expect("the narration must reach the log");
        assert!(
            written.contains("[shep]"),
            "shep's voice in a dog's log has to be marked as shep's: {written}"
        );
        assert!(
            written.contains("HAS connected to this shepherd"),
            "the verdict must say what this shepherd watched arrive: {written}"
        );
        assert!(
            written.contains("reinstalling the same build will NOT"),
            "the two days were spent on advice this line has to refuse: {written}"
        );
        assert!(
            !written.contains("cannot talk to this shep either"),
            "the sentence that cost two days must not appear on this path: {written}"
        );
        assert!(
            !written.contains("cannot reach this shep"),
            "this dog reached shep; nothing here may claim otherwise: {written}"
        );
    }
}
