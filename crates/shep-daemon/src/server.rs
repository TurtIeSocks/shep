//! The connection layer: peer auth, handshake, subscriptions
//!
//! [`RpcServer`] owns the bound [`Listener`] and accepts connections until
//! told to stop. Every accepted connection runs `handle_conn` (private — the
//! connection state machine is an implementation detail, not public API) in
//! its own task: a same-uid check ([`check_peer`], unix only), a version
//! handshake, then a read loop that decodes envelopes and hands them to
//! [`rpc::dispatch`](crate::rpc::dispatch) — the portable dispatcher Task 4
//! built, which never sees a socket or a byte.
//!
//! The OS transport lives one crate down in [`shep_core::transport`], so the
//! accept loop, the handshake and the connection state machine here are one
//! implementation over a unix socket and a Windows named pipe alike. The
//! single genuine platform difference left in this file is [`check_peer`].
//!
//! # Security
//!
//! See [`RpcServer`]'s doc for the canonical writeup; everything
//! security-relevant in this crate anchor-links there rather than repeating it.

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

/// The control socket — shep's privilege boundary
///
/// Wraps an already-bound [`Listener`] with the daemon-wide [`RpcContext`]
/// and drives the accept loop.
///
/// # Security
///
/// This is the canonical security writeup for the whole daemon (IR-29):
/// every other module that touches something security-relevant links back
/// here instead of re-arguing it.
///
/// Design criteria: `$SHEP_HOME/run` is *intended* to be created `0700` so
/// no other user can reach the socket path at all — that creation (and any
/// permission check of an already-existing `run` dir) is the daemon's boot
/// path's responsibility ([`crate::boot::init_dirs`], Task 6), not this
/// module's: nothing in [`RpcServer`] creates, checks, or enforces that
/// mode, it only accepts on whatever listener it is handed. Every accepted
/// connection is checked with `SO_PEERCRED`/`getpeereid` ([`check_peer`])
/// and refused unless the peer's uid equals the daemon's; this fails
/// CLOSED — a connection whose credentials the OS will not report
/// ([`AuthError::NoCredentials`]) is refused, not admitted, exactly like a
/// confirmed uid mismatch.
///
/// **Both of those sentences describe the unix tier.** Windows reaches the
/// same posture by a different route and one step earlier: there is no
/// `0700` directory and no post-accept credential check, because the control
/// pipe's own ACL denies a foreign local user the open-for-write that
/// speaking this protocol requires, and the OS enforces that at `CreateFile`
/// time before any byte reaches this module.
/// [`shep_core::transport`]'s module doc is the canonical account of that
/// difference, including what it does and does not cover.
///
/// The handshake refuses protocol skew with a typed
/// error ([`RpcErrorCode::ProtocolMismatch`]) rather than silence; frames
/// are capped at [`shep_core::protocol::MAX_FRAME_BYTES`]. Every
/// peer-supplied *pattern* is bounded before it can cost the daemon
/// unbounded work compiling it: a `Subscribe`'s topic-glob *count* (not the
/// byte length of any individual pattern, which is unbounded short of
/// `MAX_FRAME_BYTES`) is capped at [`crate::bus::MAX_TOPIC_PATTERNS`], and a
/// `/regex/` [`shep_core::protocol::SelectorSpec`] — on every verb that
/// carries one, without exception — is capped at a 1 MiB compiled size by
/// [`shep_core::selector::ProcessSelector`]'s `TryFrom<SelectorSpec>` impl,
/// which is the single door from the wire type to the matcher.
/// Every call carries
/// a clamped deadline ([`crate::rpc::budget`]). The one place this daemon
/// writes secrets to disk — an app's `env`, verbatim, so a muster restore
/// can reproduce it (spec §10 redacts them everywhere else) — is
/// [`crate::snapshot::write_atomic`]'s `flock.json`, created owner-only
/// (`0600`, Task 3) and kept there across its atomic rename.
///
/// A separate, install-time trust boundary: the CLI that daemonizes this
/// process hands it a readiness-pipe descriptor over `SHEP_READY_FD` (spec
/// §3), adopted through [`crate::sys::adopt_fd`] — this crate's only
/// `unsafe fn`, and (as of Decision 1, 2026-08-08) its ONLY unsafe surface,
/// full stop: [`crate::boot::boot`] receives the already-adopted
/// [`std::fs::File`] via [`crate::boot::BootOptions::ready_fd`] and never
/// touches a raw descriptor itself (`sys.rs`'s own doc has the full
/// test-call-site accounting). That boundary sits between this process and
/// its own parent, not between this process and an RPC peer — nothing
/// arriving over the control socket can reach it — and a hostile or stale
/// descriptor is refused (below fd 3, or not currently open) rather than
/// adopted; see [`crate::sys`]'s own rationale essay for the full threat
/// model.
///
/// Explicit non-goals: root can always read daemon memory; a peer with the
/// same uid is fully trusted (it could simply run the binary itself); there
/// is no post-handshake idle timeout (a same-uid peer that completes the
/// handshake and then never sends another frame holds its connection, and
/// this module's queue/task resources, open indefinitely); and there is no
/// cap on the number of concurrent connections one uid can hold open
/// ([`RpcServer::serve`] spawns and detaches unconditionally on every
/// accepted connection).
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
    /// The accept loop is a `select!` over `listener.accept()` and
    /// `shutdown.changed()` — both cancel-safe, so neither branch loses
    /// state when the other resolves first. A transient accept error (e.g.
    /// `EMFILE`) is logged and the loop continues: one bad `accept()` must
    /// not take the whole daemon down.
    ///
    /// Each accepted connection's task is spawned and detached — this loop
    /// does not track or await it. That is fine for `serve` itself (it never
    /// blocks a still-running connection), but it means `serve` returning is
    /// not a guarantee that every in-flight connection has finished; a
    /// future daemon-shutdown sequence that needs to *drain* live
    /// connections before exiting will need its own seam here (a
    /// `tokio::task::JoinSet` in place of the bare `tokio::spawn`), not
    /// yet built.
    pub async fn serve(self, mut shutdown: watch::Receiver<bool>) {
        // `mut` because `Listener::accept` needs `&mut self` on both
        // platforms: a Windows named pipe server instance is consumed by
        // whoever connects to it, so accepting means handing that instance
        // out and creating the next one. See `shep_core::transport::Listener`.
        let Self { mut listener, ctx } = self;
        // A shutdown signal that was ALREADY `true` before this loop's first
        // `changed()` call would otherwise never be observed: `changed()`
        // only resolves on a value newer than the one this receiver has
        // last seen, and a receiver in fresh-from-the-daemon condition
        // hasn't seen anything past the value it was constructed with.
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
                    // An `Err` here means the sender dropped — treat that the
                    // same as an explicit `true`: either way, stop serving.
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
// Peer-credential decision (deviation, deliberate): this uses
// `tokio::net::UnixStream::peer_cred()`, not
// `nix::sys::socket::getsockopt(PeerCredentials)`. nix 0.29 gates
// `PeerCredentials` behind `#[cfg(linux_android)]` — on macOS, a tier-1
// platform (spec §11), that sockopt does not exist and the daemon would not
// compile. tokio's `UCred` already dispatches to `SO_PEERCRED` on Linux,
// `getpeereid` on macOS/BSD, and `LOCAL_PEERCRED`/`getpeerucred` elsewhere,
// needs no new dependency, and adds no unsafe. nix is still used for
// `geteuid()` below, which has no such split.
///
/// # Errors
/// - [`AuthError::NoCredentials`] — the OS refused to report peer credentials.
/// - [`AuthError::ForeignUid`] — the peer's uid is not the daemon's.
///
/// # Platform
///
/// Unix only. There is no Windows counterpart, and there is not meant to be
/// one: see [`handle_conn`]'s own comment and
/// [`shep_core::transport`]'s module doc for why the pipe's ACL answers this
/// question earlier than any post-accept check could.
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
/// # Why this is not folded into [`check_peer`]
///
/// It reads the same [`UCred`](tokio::net::unix::UCred) that function
/// already throws all but the uid away from, so folding the two would save
/// one `getsockopt` per connection. It stays separate because `check_peer`
/// is an AUTHORISATION decision — its answer either admits the peer or ends
/// the connection — and this is a diagnostic that must never do either. A
/// pid the OS declines to report is a `None` here and nothing more; making
/// it a `check_peer` variant would put a refusal one careless `?` away from
/// a question that has no security content at all. The saved syscall is
/// paid back many times over by the handshake round trip that follows.
///
/// # What `None` means, and what it does not
///
/// Not "no process is there" — that peer exists and has already passed the
/// uid check. It means the platform has no answer: tokio fills the pid on
/// Linux, macOS, the BSDs, Solaris and several more, and returns `None`
/// everywhere else. Callers must degrade honestly rather than treat it as a
/// fact, which is what [`Contact::Unknown`](crate::dogs::Contact::Unknown)
/// exists to make hard to get wrong.
///
/// # Platform
///
/// Unix only, alongside [`check_peer`]. There is no Windows counterpart and
/// there is not meant to be one: `shep_core::transport`'s module doc argues
/// that establishing an admitted peer's identity there would need
/// `ImpersonateNamedPipeClient` and raw FFI, which `#![forbid(unsafe_code)]`
/// does not permit.
#[cfg(unix)]
#[must_use]
pub fn peer_pid(stream: &tokio::net::UnixStream) -> Option<u32> {
    // Every failure is the same answer: an OS that would not say. A pid
    // wide enough to overflow `u32` is not a thing any of these platforms
    // produces, and inventing one here rather than dropping it would be
    // worse than admitting ignorance.
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
/// `#[non_exhaustive]`: today's two variants cover a credentials read that
/// failed outright and one that succeeded but named the wrong uid, and a
/// future check — a group-membership or TLS peer-certificate requirement —
/// would need its own variant rather than stretching [`Self::ForeignUid`] to
/// mean something it does not, and shep-daemon is a published library an
/// out-of-tree matcher should not break for (IR-20).
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
/// Every variant is terminal: the connection layer logs it (via
/// [`RpcServer::serve`]'s spawn) and closes the socket. None of these panic
/// the daemon — a malformed or hostile peer can only ever cost itself its
/// own connection.
///
/// `#[non_exhaustive]`: the connection layer already distinguishes seven
/// failure points across auth, framing, encode/decode, and handshake
/// timing, and a future one — a TLS handshake failure, or a rate-limit
/// refusal — would add its own variant rather than overloading
/// [`Self::Auth`], which is specifically [`check_peer`]'s verdict, and
/// shep-daemon is a published library an out-of-tree matcher should not
/// break for (IR-20).
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
    /// The connection's write queue is gone — the writer task exited.
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

// `Decode` and `Encode` both wrap `WireError`, so only one of them could
// ever claim `impl From<WireError> for ConnError`: the compiler forbids a
// second one for the same source type, and picking one anyway would make a
// bare `?` silently mislabel the other direction. Both stay explicit
// `map_err` calls; see this task's own report.

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
    // Unix only, and its absence on Windows is a deliberate design decision
    // rather than a gap — `shep_core::transport`'s module doc is the
    // canonical writeup. In short: on unix the peer is admitted by the
    // filesystem (`0700` on `$SHEP_HOME/run`) and this is the second layer
    // behind that; on Windows the pipe's own ACL refuses a foreign user's
    // open-for-write before a byte reaches this function, so the equivalent
    // check has already happened, earlier and in the kernel. Adding a
    // post-accept check there would need `ImpersonateNamedPipeClient` and
    // raw FFI to re-answer a question the OS already answered.
    #[cfg(unix)]
    check_peer(&stream, daemon_uid())?;
    // Read once here, off the stream, and carried down to the handshake:
    // this is the whole of what lets the silence ladder tell a dog that
    // never reached the socket from one that reached it and would not say
    // who it was. `None` on Windows, and on any unix whose OS declines to
    // name a pid — see `peer_pid`, and `dogs::Contact::Unknown` for what a
    // reader must do with that.
    #[cfg(unix)]
    let peer = peer_pid(&stream);
    #[cfg(not(unix))]
    let peer: Option<u32> = None;
    // Recorded BEFORE a byte is read, deliberately: a peer that connects
    // and then says nothing at all has still reached this daemon, and that
    // is exactly the fact the ladder is missing today.
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

    // Drop the queue's sender and JOIN the writer before returning, on EVERY
    // path: a protocol-skew refusal is written by that task, so returning the
    // error early would close the socket before the client ever saw why.
    drop(out_tx);
    let _ = writer.await;
    // Beside the two lines above for the same reason their comment gives:
    // this block is on EVERY path out. A smit belongs to the connection that
    // painted it, and this is the one place that is true of. After the writer
    // join, so a client that painted and immediately read still sees its own
    // mark in the reply it was already sent.
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
    // EVERY path out of read_loop — Ok or any `?`-propagated Err — lands
    // here: a live forwarder MUST be aborted, not just dropped. Dropping a
    // JoinHandle detaches the task rather than stopping it, and a detached
    // forwarder keeps holding its own clone of `out` forever, which keeps
    // write_loop's `rx.recv()` from ever observing every sender gone —
    // which hangs `handle_conn`'s `writer.await` forever, which leaks the
    // task and the connection's fds and never actually closes the socket.
    // (Regression: see `a_garbage_frame_after_subscribing_still_closes_the_connection`.)
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
    // Ahead of the protocol check, and that ordering is the point: this
    // records what the peer SENT, not what this daemon made of it. A dog
    // refused on protocol skew still named itself, and a diagnosis that
    // forgot so would put it back in the anonymous pile it does not belong
    // in.
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
            // The refusal names our PROTOCOL, which does not say which shep
            // is running. `shep daemon reload` chooses its mechanism by
            // version, and this is the one path where the ack that would
            // have carried it never arrives. Same field the ack uses, so a
            // client cannot learn two versions for one daemon.
            daemon_version: Some(ctx.daemon_version.clone()),
        });
        send(out, &refusal).await?;
        // The refusal is on the writer's queue before anything else
        // happens, so a peer that is about to be restarted still learns
        // why. Then: which dog was that, and what does this daemon owe it?
        //
        // `None` is every client that is not a dog, and the CLI is nearly
        // all of them. A `shep` verb has no way to name a dog here — the
        // name travels only on `ReconnectingClient`, the type dogs use and
        // no verb does — so the branch below cannot be reached by an
        // operator with a stray `$SHEP_DOG_NAME` in their environment.
        match &hello.dog_name {
            Some(dog) => {
                crate::dogs::record_refused_dog(
                    dog,
                    &hello.client_version,
                    &ctx.dog_refusals,
                    &ctx.supervisor,
                );
            }
            // A refused client this daemon cannot restart and would not
            // want to: an operator running an older `shep` against a newer
            // daemon, who is already reading the skew in plain English from
            // their own CLI. `debug!` rather than `warn!`, which this was
            // until a real reload across a protocol bump measured it: the
            // CLI polls the socket while it waits for a successor, so ONE
            // `shep daemon reload` produced 442 of these in 9.8 seconds,
            // and the daemon's default level is `warn`. The only fact this
            // adds over `handle_conn`'s own "connection ended" line is the
            // client's crate version, which is worth carrying and is not
            // worth waking anybody for.
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
    // A dog that got in is not stale, whatever this daemon held against it
    // before — including the restart it was just given, which is exactly
    // the case that has to clear.
    if let Some(dog) = &hello.dog_name {
        ctx.dog_refusals.handshook(dog);
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
    // Real time: every test here drives a real UnixStream. Under a paused
    // clock the runtime auto-advances whenever it goes idle, which can expire
    // HANDSHAKE_TIMEOUT_MS before the peer's bytes are delivered.
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
    /// A real transport on both platforms — a socketpair on unix, an actual
    /// named pipe on Windows — rather than an in-memory duplex, because
    /// several tests below turn on what a peer sees when the other side
    /// closes, which only a real transport reproduces. `async` (it was not,
    /// when it could call the synchronous `UnixStream::pair`) because
    /// creating a pipe pair means connecting one, and every caller is
    /// already inside a `#[tokio::test]`.
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
        let h = harness(vec![]); // same helper shape as rpc.rs's tests
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
        // The refusal reports the daemon's PROTOCOL, which says nothing
        // about which crate version is running. `shep daemon reload` picks
        // between a handover and a stop-and-start by version, and a protocol
        // bump is exactly when it is needed, so the refusal has to name it.
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
        // The same string the ack would have carried, from the same field:
        // a client must never learn two different versions for one daemon
        // depending on whether its handshake was accepted.
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

        // Back-to-back arrival is the filtering assertion — no negative wait.
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
        // Regression test (Opus security review, post-Task-5): a live
        // forwarder holds its own clone of `out`. If a connection error
        // (garbage frame, oversize frame, ...) after Subscribe skipped
        // aborting that forwarder, `out_tx`'s drop in `handle_conn` would NOT
        // be the last sender — write_loop's `rx.recv()` would never see
        // every sender go away, `handle_conn`'s `writer.await` would hang
        // forever, and the socket would never actually close. Subscribing
        // first is what makes that path reachable; the plain
        // `a_garbage_frame_ends_the_connection_without_panicking` test above
        // never subscribes, so it could not have caught this.
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

    /// `cfg(unix)`, like [`check_peer`] itself. Windows has no counterpart
    /// to gate: the pipe's ACL refuses a foreign user's open before
    /// `handle_conn` is reached at all, so there is no post-accept decision
    /// here for a test to exercise. See `shep_core::transport`'s module doc.
    #[cfg(unix)]
    #[tokio::test]
    async fn peer_credentials_gate_on_uid() {
        // What this DOES prove: check_peer's own uid-comparison and error
        // construction are correct — same-uid accepts and hands back the
        // uid, a mismatched daemon_uid is refused as ForeignUid with both
        // uids recorded.
        //
        // What this does NOT prove: that a connection from an actual
        // different-uid OS process is rejected. `UnixStream::pair()` always
        // reports both ends as owned by this test process's own uid (there is
        // no way to fake the peer side of `SO_PEERCRED`/`getpeereid` — the
        // kernel derives it from the real socket, and `tokio::net::unix::UCred`
        // has no public constructor for a synthetic one), so this test can
        // only vary the `daemon_uid` argument, never the peer's true uid.
        // Exercising the real cross-uid path needs a second OS user (root in
        // CI, or two accounts) actually connecting — out of reach for this
        // crate's test harness; see the report's security-review note.
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
        // Adversarial finding #3: bus.rs's `step()` unit test proves the
        // Lagged->Dropped translation in isolation, but nothing before this
        // exercised it through the REAL connection stack — CONN_QUEUE
        // filling, the forwarder parking on `out.send`, and the broadcast
        // ring (bus.rs's BUS_CAPACITY) actually overflowing for a subscriber
        // that truly never reads. Real time: real socket, matching this
        // whole test mod's rule.
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

        // Never call client.recv() again until AFTER the flood: CONN_QUEUE
        // fills, the forwarder blocks on `out.send`, and the broadcast ring
        // (BUS_CAPACITY) takes the overflow from there — the exact
        // back-pressure chain bus.rs's module comment documents.
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

        // Resume reading. The count comes from tokio's own Lagged(n) inside
        // the forwarder, never hand-computed here (no-hand-computed-sequences
        // rule) — this only asserts a Dropped notice arrives and is nonzero.
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
    // A refused handshake never reaches a request, so `Request::DogConfig`'s
    // name — the one place a dog otherwise identifies itself — is
    // unreachable on exactly the path where the daemon has to know which
    // dog it just refused. `Hello.dog_name` is what closes that, and these
    // four tests are what the daemon does with it.

    /// Registers `name` as a built-in dog and returns the row it produced.
    ///
    /// Straight through [`crate::supervisor::SupervisorHandle::start_dog`]
    /// rather than through `Request::EnableDog`, because the request path
    /// would need a handshaken connection of its own and the point here is
    /// what a REFUSED one does.
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
    /// The close is the forcing mechanism for everything synchronous
    /// (IR-46): the daemon records the refusal and decides what it owes the
    /// dog BEFORE it returns the error that closes the socket, so a caller
    /// that has seen the close can read the verdict without racing it. The
    /// restart itself runs on its own task and needs [`await_dog`].
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
    /// handle to await; polling a real condition against a hard ceiling is
    /// the forcing mechanism (IR-46). The elapsed time is returned because
    /// the never-restart-twice test below sizes its negative window against
    /// it rather than against a guess.
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

    /// fails if a refused dog is left mute. This is G8's step 2, and the
    /// daemon is the only party that can take it: the dog's own client has
    /// stopped rather than spinning (that is `ReconnectingClient`'s half),
    /// and a dog carried across a handover is a live process holding a dead
    /// socket that nothing else will replace.
    ///
    /// The pid moving is the assertion, not the restart count: a restart
    /// that re-registered the row without re-spawning would leave a dog
    /// exactly as mute as it was.
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

    /// fails if the daemon restarts a dog it has already restarted — the
    /// spin G8 exists to forbid. A second refusal proves the binary on disk
    /// cannot satisfy this daemon either, because the restart already ran
    /// it, so a third attempt is not optimism.
    ///
    /// Three independent things have to hold, and each catches a different
    /// mutation. The dog must be REPORTED stale, which a daemon that
    /// silently stopped restarting would fail. Its pid must not move inside
    /// a window sized against the restart that really happened earlier in
    /// this same test — a negative assertion is only as good as its window,
    /// and a measured one scales with a loaded runner where a fixed number
    /// would not. And the harness is scripted with exactly the two spawns
    /// G8 permits, so a third would fail to spawn and take the dog out of
    /// `Online` even if it somehow beat the window.
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

        // Ten times the restart that DID happen, floored so a machine fast
        // enough to make the measurement meaningless still watches for a
        // real interval.
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

    /// fails if a dog that got back in stays condemned. The restart G8 owes
    /// a refused dog is worth nothing if the successful handshake it
    /// produces does not clear the mark: the dog would be reported stale
    /// forever while answering perfectly, and a LATER daemon that refused
    /// it would skip straight past its one restart.
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

    /// fails if an operator running an older `shep` has a dog restarted
    /// under them. The CLI cannot name a dog — the name travels only on
    /// `ReconnectingClient`, which no verb uses — so a refusal carrying no
    /// name must leave the flock exactly as it was, however many of them
    /// arrive.
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
}
