//! Hand-rolled test doubles shared across this phase's tasks (and, via the
//! `test-support` feature, shep-cli's own tests) — the ONE home for every
//! scripted daemon/client double: no other module grows a second
//! `fake_daemon`, and no other crate defines its own.
//!
//! Every helper takes the socket path as `&Path` — this module carries no
//! dev-dependencies (it compiles into an ordinary build under
//! `test-support`, so `missing_docs` and `missing_debug_implementations`
//! apply to it exactly like any other public module), so the caller owns
//! the `TempDir`.
//!
//! [`fake_daemon`], [`sample_ack`], and [`sample_info`] are the
//! handshake-only primitives. [`FakeDaemon`] and the `fake_client_*`
//! helpers below it connect a real [`Client`] against a scripted peer,
//! for testing the connection actor's request/reply routing and beyond.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures_util::{SinkExt, StreamExt};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::codec::Framed;

use shep_core::protocol::{
    BusEvent, Envelope, Hello, HelloAck, HelloReply, PROTOCOL_VERSION, ProcessEventKind,
    ProcessInfo, Reply, Request, Response, RpcError, RpcErrorCode, codec, decode_frame,
    encode_frame,
};
use shep_core::status::ProcStatus;

use crate::Client;
use crate::connection::Frames;

/// Serves exactly one connection, replying to the `Hello` with `reply`,
/// then closing. The returned handle yields the `Hello` the client
/// actually sent, so a test can assert on the announcement as well as on
/// the answer.
///
/// Binds before returning, so a caller that awaits it can `connect`
/// immediately without a sleep.
///
/// Panics if `path` cannot be bound, or if the served connection fails to
/// accept, read the `Hello` frame, decode it, or send `reply` back — this
/// is test scaffolding, meant to fail the test loudly rather than surface
/// a `Result` nobody would check.
pub async fn fake_daemon(path: &Path, reply: HelloReply) -> JoinHandle<Hello> {
    let listener = UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut frames = Framed::new(stream, codec());
        let first = frames.next().await.unwrap().unwrap();
        let hello: Hello = decode_frame(&first).unwrap();
        frames.send(encode_frame(&reply).unwrap()).await.unwrap();
        hello
    })
}

/// A `HelloAck` with a distinctive version and pid, so a test that asserts
/// on either can tell a real read from a default.
#[must_use]
pub fn sample_ack() -> HelloAck {
    HelloAck {
        daemon_version: "9.9.9".into(),
        protocol: PROTOCOL_VERSION,
        pid: 4242,
    }
}

/// One fully-populated [`ProcessInfo`] — every `Option` is `Some`, so a
/// payload type's anti-drift test sees every serialized field.
#[must_use]
pub fn sample_info() -> ProcessInfo {
    ProcessInfo {
        id: 1,
        name: "web".to_string(),
        status: ProcStatus::Online,
        pid: Some(4242),
        restarts: 3,
        uptime_ms: 60_000,
        fold: Some("backend".to_string()),
    }
}

/// Depth of a [`FakeDaemon`]'s script channel and of
/// [`fake_client_capturing_envelopes`]'s capture channel — generous test
/// scaffolding, never tuned, same rationale as `shep-daemon`'s own fake
/// (`fake.rs`'s `CHANNEL_CAPACITY`).
const SCRIPT_CHANNEL_CAPACITY: usize = 8;

/// Completes the handshake side of the protocol: reads the client's
/// `Hello` (and discards it — callers that need to assert on it use
/// [`fake_daemon`] instead) and answers with `ack`.
///
/// Panics on any accept/read/decode/write failure — test scaffolding, see
/// [`fake_daemon`]'s own doc for why that is the right failure mode here.
async fn handshake(frames: &mut Frames, ack: HelloAck) {
    let first = frames.next().await.unwrap().unwrap();
    let _hello: Hello = decode_frame(&first).unwrap();
    let reply: HelloReply = Ok(ack);
    frames.send(encode_frame(&reply).unwrap()).await.unwrap();
}

/// Reads and decodes the next envelope. Panics on failure or a closed
/// connection — test scaffolding, see [`fake_daemon`]'s own doc.
async fn read_envelope(frames: &mut Frames) -> Envelope {
    let frame = frames.next().await.unwrap().unwrap();
    decode_frame(&frame).unwrap()
}

/// Encodes and sends one successful [`Reply`] for `id`. Panics on failure —
/// test scaffolding, see [`fake_daemon`]'s own doc.
async fn write_reply(frames: &mut Frames, id: u64, response: Response) {
    let reply = Reply {
        id,
        result: Ok(response),
    };
    frames.send(encode_frame(&reply).unwrap()).await.unwrap();
}

/// One scripted step sent to a [`FakeDaemon`]'s background task.
enum ScriptCommand {
    /// Arms the answer to the next `Request::ListFlock`.
    ReplyToList(Vec<ProcessInfo>),
    /// Ends the script: stop serving and let the task return.
    Close,
}

/// A scripted daemon over one accepted connection.
///
/// [`Self::reply_to_list`] arms the answer to the next `Request::ListFlock`;
/// [`Self::list_flock_count`] reports how many `ListFlock` requests this
/// connection has answered, so a test can prove a client cached rather than
/// re-asked; [`Self::close`] ends the script and drops the connection.
/// Every other request this fake receives is answered with
/// `Response::Pong` — good enough for a test that only cares about
/// `ListFlock` behavior, or about a request getting *some* prompt reply.
#[derive(Debug)]
pub struct FakeDaemon {
    script: mpsc::Sender<ScriptCommand>,
    list_flock_count: Arc<AtomicU64>,
    task: JoinHandle<()>,
}

impl FakeDaemon {
    /// Arms the answer to the next `Request::ListFlock` this connection
    /// receives.
    ///
    /// Panics if the background task is gone — test scaffolding, see
    /// [`fake_daemon`]'s own doc.
    pub async fn reply_to_list(&self, flock: Vec<ProcessInfo>) {
        self.script
            .send(ScriptCommand::ReplyToList(flock))
            .await
            .unwrap();
    }

    /// How many `Request::ListFlock` envelopes this connection has
    /// answered so far.
    #[must_use]
    pub fn list_flock_count(&self) -> u64 {
        self.list_flock_count.load(Ordering::SeqCst)
    }

    /// Ends the script and drops the connection: drains anything still
    /// queued, then lets the background task finish.
    ///
    /// Panics if the background task is gone or panicked — test
    /// scaffolding, see [`fake_daemon`]'s own doc.
    pub async fn close(self) {
        let _ = self.script.send(ScriptCommand::Close).await;
        self.task.await.unwrap();
    }
}

/// The [`FakeDaemon`] background task: accepts one connection, handshakes
/// with `ack`, then answers requests (`ListFlock` per the armed script,
/// everything else with `Response::Pong`) until [`ScriptCommand::Close`]
/// arrives or the connection ends.
async fn serve_scripted(
    listener: UnixListener,
    ack: HelloAck,
    mut script: mpsc::Receiver<ScriptCommand>,
    list_flock_count: Arc<AtomicU64>,
) {
    let (stream, _) = listener.accept().await.unwrap();
    let mut frames = Framed::new(stream, codec());
    handshake(&mut frames, ack).await;

    let mut armed_list: Option<Vec<ProcessInfo>> = None;
    loop {
        tokio::select! {
            command = script.recv() => {
                match command {
                    Some(ScriptCommand::ReplyToList(flock)) => armed_list = Some(flock),
                    Some(ScriptCommand::Close) | None => break,
                }
            }
            frame = frames.next() => {
                let Some(Ok(frame)) = frame else { break };
                let envelope: Envelope = decode_frame(&frame).unwrap();
                let response = if matches!(envelope.body, Request::ListFlock) {
                    list_flock_count.fetch_add(1, Ordering::SeqCst);
                    Response::Flock(armed_list.take().unwrap_or_default())
                } else {
                    Response::Pong
                };
                write_reply(&mut frames, envelope.id, response).await;
            }
        }
    }
}

/// Binds `path`, handshakes with [`sample_ack`], and hands back a connected
/// [`Client`] alongside the still-live [`FakeDaemon`] script — for a test
/// that only needs a client past the handshake and nothing daemon-specific.
pub async fn fake_client_on(path: &Path) -> (Client, FakeDaemon) {
    fake_client_with_ack(path, sample_ack()).await
}

/// As [`fake_client_on`], but with a caller-chosen [`HelloAck`] — for a
/// test asserting on the ack a `Client` receives.
pub async fn fake_client_with_ack(path: &Path, ack: HelloAck) -> (Client, FakeDaemon) {
    let listener = UnixListener::bind(path).unwrap();
    let (script_tx, script_rx) = mpsc::channel(SCRIPT_CHANNEL_CAPACITY);
    let list_flock_count = Arc::new(AtomicU64::new(0));
    let task = tokio::spawn(serve_scripted(
        listener,
        ack,
        script_rx,
        Arc::clone(&list_flock_count),
    ));
    let client = Client::connect(path).await.unwrap();
    (
        client,
        FakeDaemon {
            script: script_tx,
            list_flock_count,
            task,
        },
    )
}

/// Binds `path`, handshakes with [`sample_ack`], and answers every request
/// with `Response::Pong` while forwarding each decoded [`Envelope`] onto
/// the returned channel — for a test asserting on what a `Client` actually
/// puts on the wire (its deadline, in particular) rather than on how the
/// daemon answers.
pub async fn fake_client_capturing_envelopes(path: &Path) -> (Client, mpsc::Receiver<Envelope>) {
    let listener = UnixListener::bind(path).unwrap();
    let (tx, rx) = mpsc::channel(SCRIPT_CHANNEL_CAPACITY);
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut frames = Framed::new(stream, codec());
        handshake(&mut frames, sample_ack()).await;
        loop {
            let envelope = read_envelope(&mut frames).await;
            let id = envelope.id;
            // Forwarded before the reply is sent, so a test that awaits the
            // `Client::request` future first is guaranteed to find its
            // envelope already sitting in the channel.
            if tx.send(envelope).await.is_err() {
                break;
            }
            write_reply(&mut frames, id, Response::Pong).await;
        }
    });
    let client = Client::connect(path).await.unwrap();
    (client, rx)
}

/// Binds `path`, handshakes with [`sample_ack`], and answers the one
/// request that arrives with `RpcError { code, message }` instead of a
/// normal response — for testing that a daemon-side error reply surfaces
/// through `Client::request` as `RequestError::Rpc`.
pub async fn fake_client_replying_err(
    path: &Path,
    code: RpcErrorCode,
    message: &str,
) -> (Client, JoinHandle<()>) {
    let listener = UnixListener::bind(path).unwrap();
    let message = message.to_string();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut frames = Framed::new(stream, codec());
        handshake(&mut frames, sample_ack()).await;
        let envelope = read_envelope(&mut frames).await;
        let reply = Reply {
            id: envelope.id,
            result: Err(RpcError { code, message }),
        };
        frames.send(encode_frame(&reply).unwrap()).await.unwrap();
    });
    let client = Client::connect(path).await.unwrap();
    (client, task)
}

/// Binds `path`, handshakes with [`sample_ack`], reads exactly two
/// envelopes, then answers them in REVERSE arrival order — the `ListFlock`
/// one first, with `Response::Flock(vec![])`, then the `Ping` one, with
/// `Response::Pong` — proof that a `Client` routes replies by id rather
/// than by the order it sent the requests in.
pub async fn fake_client_out_of_order(path: &Path) -> (Client, JoinHandle<()>) {
    let listener = UnixListener::bind(path).unwrap();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut frames = Framed::new(stream, codec());
        handshake(&mut frames, sample_ack()).await;
        let first = read_envelope(&mut frames).await;
        let second = read_envelope(&mut frames).await;
        let (list_env, ping_env) = if matches!(first.body, Request::ListFlock) {
            (first, second)
        } else {
            (second, first)
        };
        write_reply(&mut frames, list_env.id, Response::Flock(Vec::new())).await;
        write_reply(&mut frames, ping_env.id, Response::Pong).await;
    });
    let client = Client::connect(path).await.unwrap();
    (client, task)
}

/// Binds `path`, handshakes with [`sample_ack`], reads one envelope, and
/// sends a `BusEvent::Process` event BEFORE answering it — the daemon's own
/// documented ordering (`shep-daemon/tests/daemon_e2e.rs:161-174`): a
/// sheep's bus event can legitimately arrive ahead of the reply for the
/// very request that caused it.
pub async fn fake_client_event_then_reply(path: &Path) -> (Client, JoinHandle<()>) {
    let listener = UnixListener::bind(path).unwrap();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut frames = Framed::new(stream, codec());
        handshake(&mut frames, sample_ack()).await;
        let envelope = read_envelope(&mut frames).await;
        let event = BusEvent::Process {
            event: ProcessEventKind::Online,
            info: sample_info(),
            manually: false,
            at_ms: 0,
        };
        frames.send(encode_frame(&event).unwrap()).await.unwrap();
        write_reply(&mut frames, envelope.id, Response::Pong).await;
    });
    let client = Client::connect(path).await.unwrap();
    (client, task)
}

/// Binds `path`, handshakes with [`sample_ack`], then immediately drops the
/// connection — for testing that a `Client` fails every pending request
/// with `RequestError::Closed` rather than hanging.
pub async fn fake_client_that_closes_after_handshake(path: &Path) -> (Client, JoinHandle<()>) {
    let listener = UnixListener::bind(path).unwrap();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut frames = Framed::new(stream, codec());
        handshake(&mut frames, sample_ack()).await;
        // Dropping `frames` (and the `UnixStream` it owns) here closes the
        // connection from this side.
    });
    let client = Client::connect(path).await.unwrap();
    (client, task)
}

/// Binds `path`, handshakes with [`sample_ack`], then reads nothing and
/// replies to nothing, ever — for testing a `Client`'s own client-side
/// deadline against a daemon that accepted the connection but stopped
/// answering.
pub async fn fake_client_that_never_replies(path: &Path) -> (Client, JoinHandle<()>) {
    let listener = UnixListener::bind(path).unwrap();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut frames = Framed::new(stream, codec());
        handshake(&mut frames, sample_ack()).await;
        core::future::pending::<()>().await;
    });
    let client = Client::connect(path).await.unwrap();
    (client, task)
}
