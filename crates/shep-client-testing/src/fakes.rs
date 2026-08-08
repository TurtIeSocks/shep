//! The fakes themselves; the crate root documents the set and re-exports it.
//!
//! Everything here is built from ordinary dependencies — `tokio`,
//! `tokio-util`, `futures-util`, `shep_core::protocol` and `std`. This crate
//! has no dev-dependencies at all, so nothing in it may reach for one; in
//! particular there is no `tempfile`, which is why every helper takes the
//! socket path as a `&Path` and the caller owns the `TempDir`.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use shep_core::protocol::{
    BusEvent, Envelope, Hello, HelloAck, HelloReply, PROTOCOL_VERSION, ProcessEventKind,
    ProcessInfo, Reply, Request, Response, RpcError, RpcErrorCode, codec, decode_frame,
    encode_frame,
};
use shep_core::status::ProcStatus;

use shep_client::{Client, EVENT_CHANNEL_CAPACITY};

/// The framed transport a fake serves its one connection over.
///
/// Deliberately our own alias rather than shep-client's `Frames`: that one is
/// `pub(crate)` and must stay that way, since exporting it would pin
/// tokio-util's `Framed` into shep-client's public API and tie the crate to
/// that dependency's major version. A fake only ever needs the *daemon* side
/// of the wire, which it builds itself from the public `codec()` — so nothing
/// here needs shep-client to widen anything.
type Frames = Framed<UnixStream, LengthDelimitedCodec>;

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

/// Encodes and sends one error [`Reply`] for `id`. Panics on failure —
/// test scaffolding, see [`fake_daemon`]'s own doc.
async fn write_err(frames: &mut Frames, id: u64, code: RpcErrorCode, message: String) {
    let reply = Reply {
        id,
        result: Err(RpcError { code, message }),
    };
    frames.send(encode_frame(&reply).unwrap()).await.unwrap();
}

/// Encodes and sends one [`BusEvent`] frame directly — not wrapped in a
/// [`Reply`], the shape a real subscriber actually receives. Panics on
/// failure — test scaffolding, see [`fake_daemon`]'s own doc.
async fn write_event(frames: &mut Frames, event: BusEvent) {
    frames.send(encode_frame(&event).unwrap()).await.unwrap();
}

/// Sends a `BusEvent::Process` built from [`sample_info`] — the daemon's
/// own documented ordering (`shep-daemon/tests/daemon_e2e.rs:161-174`): a
/// sheep's bus event can legitimately arrive ahead of the reply for the
/// request that caused it. Panics on failure — test scaffolding, see
/// [`fake_daemon`]'s own doc.
async fn send_sample_event(frames: &mut Frames) {
    write_event(
        frames,
        BusEvent::Process {
            event: ProcessEventKind::Online,
            info: sample_info(),
            manually: false,
            at_ms: 0,
        },
    )
    .await;
}

/// One scripted step sent to a [`FakeDaemon`]'s background task.
///
/// [`FakeDaemon::reply_to_list`] and [`FakeDaemon::queue_reply_then_event`]
/// do NOT go through this channel — both are synchronous (a `Mutex`-backed
/// flag), because every plan call site invokes them without `.await`. This
/// enum carries the remaining scripted behaviors, each armed at most once,
/// by the `fake_client_*` constructor or `FakeDaemon` method that needs it.
enum ScriptCommand {
    /// Arms the next request (of any kind) to receive
    /// `RpcError { code, message }` instead of a normal response.
    ReplyErr(RpcErrorCode, String),
    /// Arms the next request to receive a [`sample_info`]-based
    /// `BusEvent::Process` BEFORE its `Pong` reply.
    EventThenReply,
    /// Buffers the next two requests, then answers the `ListFlock` one
    /// first (`Response::Flock(vec![])`) and the other second
    /// (`Response::Pong`) — regardless of which one arrived first, proof
    /// that a `Client` routes replies by id rather than by send order.
    ArmOutOfOrder,
    /// Queues one [`BusEvent`] for this connection's subscriber. Buffered
    /// until the connection has observed and answered a `Request::Subscribe`
    /// — see [`FakeDaemon::push`] for why — then written straight to the
    /// wire for as long as the subscription stays live.
    PushEvent(BusEvent),
    /// Ends the script: stop serving and let the task return.
    Close,
}

/// [`ScriptCommand::ArmOutOfOrder`]'s progress: idle, armed (waiting for
/// the first of the two requests), or holding the first request while it
/// waits for the second.
enum OutOfOrder {
    /// Not armed; requests are answered by the normal script.
    Idle,
    /// Armed; the next request received is buffered rather than answered.
    Armed,
    /// The first of the two requests, buffered until the second arrives.
    Buffered(Envelope),
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
/// [`Self::push`], [`Self::overrun_by`] and [`Self::queue_reply_then_event`]
/// drive this connection's event side: a real `Request::Subscribe` is
/// always answered with `Response::Subscribed` and switches the connection
/// into forwarding pushed events straight to the wire.
///
/// A handful of `fake_client_*` constructors below
/// ([`fake_client_replying_err`], [`fake_client_out_of_order`],
/// [`fake_client_event_then_reply`]) also arm a one-shot error reply, an
/// out-of-order two-request response, or an event-before-reply, via a
/// private `ScriptCommand` — internal to this module, since nothing
/// outside it needs to arm those behaviors directly.
#[derive(Debug)]
pub struct FakeDaemon {
    script: mpsc::Sender<ScriptCommand>,
    armed_list: Arc<Mutex<Option<Vec<ProcessInfo>>>>,
    armed_reply_then_event: Arc<Mutex<Option<(Response, BusEvent)>>>,
    list_flock_count: Arc<AtomicU64>,
    task: JoinHandle<()>,
}

impl FakeDaemon {
    /// Arms the answer to the next `Request::ListFlock` this connection
    /// receives.
    ///
    /// Synchronous, not `async`: every plan call site invokes this without
    /// `.await` (`docs/writing-plans/plans/2026-08-08-shep-phase3-cli.md`,
    /// e.g. lines 2425, 2452, 2475), which would trip `unused_must_use`
    /// under `-D warnings` against an `async fn`. A `Mutex`-backed flag
    /// lets this stay a plain fn instead.
    pub fn reply_to_list(&self, flock: Vec<ProcessInfo>) {
        *self.armed_list.lock().unwrap() = Some(flock);
    }

    /// How many `Request::ListFlock` envelopes this connection has
    /// answered so far.
    #[must_use]
    pub fn list_flock_count(&self) -> u64 {
        self.list_flock_count.load(Ordering::SeqCst)
    }

    /// Arms the reply this connection sends for the very next request it
    /// receives (of any kind) as `reply`, then immediately follows it with
    /// `event` written directly to the wire — the ordering
    /// `shep-daemon/src/server.rs:357` actually produces: the `Subscribed`
    /// reply ahead of any event, by queue order, once a subscriber is
    /// installed.
    ///
    /// Synchronous for the same reason [`Self::reply_to_list`] is: every
    /// call site invokes it without `.await`.
    pub fn queue_reply_then_event(&self, reply: Response, event: BusEvent) {
        *self.armed_reply_then_event.lock().unwrap() = Some((reply, event));
    }

    /// Queues `event` for this connection's subscriber.
    ///
    /// Before this connection has observed a `Request::Subscribe` and
    /// answered it, `event` is buffered in arrival order rather than
    /// written straight through — nothing is listening for it yet, since
    /// the `broadcast::Receiver` a real `Client::subscribe` call installs
    /// does not exist until that call returns, and a `broadcast::Receiver`
    /// never sees a value sent before it existed. Once the subscription is
    /// live, `push` writes straight to the wire.
    ///
    /// Silently does nothing if the background task has already ended —
    /// the same tolerance [`Self::close`] applies to its own script send —
    /// so a `push` racing the connection closing simply vanishes rather
    /// than panicking.
    pub async fn push(&self, event: BusEvent) {
        let _ = self.script.send(ScriptCommand::PushEvent(event)).await;
    }

    /// Pushes [`EVENT_CHANNEL_CAPACITY`] `+ n` [`BusEvent::LogOut`] events in
    /// one go — enough, once a subscriber falls that far behind, to force a
    /// local lag notice rather than an ordinary delivery.
    /// [`EVENT_CHANNEL_CAPACITY`] is the client actor's own broadcast channel
    /// capacity, which shep-client publishes for exactly this reason.
    pub async fn overrun_by(&self, n: usize) {
        for i in 0..(EVENT_CHANNEL_CAPACITY + n) {
            self.push(BusEvent::LogOut {
                id: 1,
                line: i.to_string(),
            })
            .await;
        }
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
/// with `ack`, then answers requests until [`ScriptCommand::Close`] arrives
/// or the connection ends.
///
/// Per request, in priority order: a request buffered by
/// [`OutOfOrder::Buffered`] is answered together with the one that just
/// arrived; else an armed `armed_reply_then_event` or
/// [`ScriptCommand::ReplyErr`] or [`ScriptCommand::EventThenReply`] is
/// consumed and answers this one request; else a `Request::Subscribe`
/// is answered with `Response::Subscribed`, flips this connection into the
/// subscribed state, and flushes anything [`ScriptCommand::PushEvent`]
/// queued before now; else `ListFlock` is answered per
/// [`FakeDaemon::reply_to_list`] (or `Flock(vec![])` if nothing is armed);
/// else `Response::Pong`.
///
/// A [`ScriptCommand::PushEvent`] arriving outside a request turn is
/// written straight to the wire once subscribed, or buffered until then —
/// see [`FakeDaemon::push`].
async fn serve_scripted(
    listener: UnixListener,
    ack: HelloAck,
    mut script: mpsc::Receiver<ScriptCommand>,
    armed_list: Arc<Mutex<Option<Vec<ProcessInfo>>>>,
    armed_reply_then_event: Arc<Mutex<Option<(Response, BusEvent)>>>,
    list_flock_count: Arc<AtomicU64>,
) {
    let (stream, _) = listener.accept().await.unwrap();
    let mut frames = Framed::new(stream, codec());
    handshake(&mut frames, ack).await;

    let mut armed_err: Option<(RpcErrorCode, String)> = None;
    let mut armed_event_then_reply = false;
    let mut out_of_order = OutOfOrder::Idle;
    // Once a `Request::Subscribe` has been answered, a pushed event is
    // written the moment it arrives. Before that, nothing is listening on
    // the client side yet (its `broadcast::Receiver` is created inside
    // `Client::subscribe`, which has not returned), so events queue here.
    let mut subscribed = false;
    let mut pending_events: Vec<BusEvent> = Vec::new();

    loop {
        tokio::select! {
            command = script.recv() => {
                match command {
                    Some(ScriptCommand::ReplyErr(code, message)) => armed_err = Some((code, message)),
                    Some(ScriptCommand::EventThenReply) => armed_event_then_reply = true,
                    Some(ScriptCommand::ArmOutOfOrder) => out_of_order = OutOfOrder::Armed,
                    Some(ScriptCommand::PushEvent(event)) => {
                        if subscribed {
                            write_event(&mut frames, event).await;
                        } else {
                            pending_events.push(event);
                        }
                    }
                    Some(ScriptCommand::Close) | None => break,
                }
            }
            frame = frames.next() => {
                let Some(Ok(frame)) = frame else { break };
                let envelope: Envelope = decode_frame(&frame).unwrap();

                match std::mem::replace(&mut out_of_order, OutOfOrder::Idle) {
                    OutOfOrder::Armed => {
                        out_of_order = OutOfOrder::Buffered(envelope);
                    }
                    OutOfOrder::Buffered(first) => {
                        let (list_env, other_env) = if matches!(first.body, Request::ListFlock) {
                            (first, envelope)
                        } else {
                            (envelope, first)
                        };
                        write_reply(&mut frames, list_env.id, Response::Flock(Vec::new())).await;
                        write_reply(&mut frames, other_env.id, Response::Pong).await;
                    }
                    OutOfOrder::Idle => {
                        // Taken into an owned local before any `.await` below:
                        // the `MutexGuard` `.lock()` produces is not `Send`,
                        // and holding it across an await point would make
                        // this whole future not `Send` (tokio::spawn requires
                        // `Send`).
                        let reply_then_event = armed_reply_then_event.lock().unwrap().take();
                        if let Some((reply, event)) = reply_then_event {
                            write_reply(&mut frames, envelope.id, reply).await;
                            write_event(&mut frames, event).await;
                            subscribed = true;
                        } else if let Some((code, message)) = armed_err.take() {
                            write_err(&mut frames, envelope.id, code, message).await;
                        } else if armed_event_then_reply {
                            armed_event_then_reply = false;
                            send_sample_event(&mut frames).await;
                            write_reply(&mut frames, envelope.id, Response::Pong).await;
                        } else if matches!(envelope.body, Request::Subscribe { .. }) {
                            write_reply(&mut frames, envelope.id, Response::Subscribed).await;
                            subscribed = true;
                            for event in pending_events.drain(..) {
                                write_event(&mut frames, event).await;
                            }
                        } else {
                            let response = if matches!(envelope.body, Request::ListFlock) {
                                list_flock_count.fetch_add(1, Ordering::SeqCst);
                                Response::Flock(armed_list.lock().unwrap().take().unwrap_or_default())
                            } else {
                                Response::Pong
                            };
                            write_reply(&mut frames, envelope.id, response).await;
                        }
                    }
                }
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
    let armed_list = Arc::new(Mutex::new(None));
    let armed_reply_then_event = Arc::new(Mutex::new(None));
    let list_flock_count = Arc::new(AtomicU64::new(0));
    let task = tokio::spawn(serve_scripted(
        listener,
        ack,
        script_rx,
        Arc::clone(&armed_list),
        Arc::clone(&armed_reply_then_event),
        Arc::clone(&list_flock_count),
    ));
    let client = Client::connect(path).await.unwrap();
    (
        client,
        FakeDaemon {
            script: script_tx,
            armed_list,
            armed_reply_then_event,
            list_flock_count,
            task,
        },
    )
}

/// Binds `path`, handshakes with [`sample_ack`], and hands back a connected
/// [`Client`] alongside the still-live [`FakeDaemon`] script — identical to
/// [`fake_client_on`], named separately for tests whose whole point is
/// [`FakeDaemon::push`], [`FakeDaemon::overrun_by`] or
/// [`FakeDaemon::queue_reply_then_event`], so the call site reads as "a
/// daemon I'm about to push events through" rather than "a daemon with
/// nothing daemon-specific armed".
pub async fn fake_client_with_push(path: &Path) -> (Client, FakeDaemon) {
    fake_client_on(path).await
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
///
/// Backed by a [`FakeDaemon`] (armed with a private `ScriptCommand::ReplyErr`)
/// so the connection keeps serving afterward, like any other `fake_client_*`
/// helper that hands one back — a bespoke one-shot task here would die
/// after the one scripted reply, which is wrong for a later test that
/// issues a second request against the same connection.
pub async fn fake_client_replying_err(
    path: &Path,
    code: RpcErrorCode,
    message: &str,
) -> (Client, FakeDaemon) {
    let (client, daemon) = fake_client_on(path).await;
    daemon
        .script
        .send(ScriptCommand::ReplyErr(code, message.to_string()))
        .await
        .unwrap();
    (client, daemon)
}

/// Binds `path`, handshakes with [`sample_ack`], reads exactly two
/// envelopes, then answers them in REVERSE arrival order — the `ListFlock`
/// one first, with `Response::Flock(vec![])`, then the `Ping` one, with
/// `Response::Pong` — proof that a `Client` routes replies by id rather
/// than by the order it sent the requests in.
///
/// Backed by a [`FakeDaemon`] (armed with a private `ScriptCommand::ArmOutOfOrder`);
/// see [`fake_client_replying_err`]'s own doc for why that matters.
pub async fn fake_client_out_of_order(path: &Path) -> (Client, FakeDaemon) {
    let (client, daemon) = fake_client_on(path).await;
    daemon
        .script
        .send(ScriptCommand::ArmOutOfOrder)
        .await
        .unwrap();
    (client, daemon)
}

/// Binds `path`, handshakes with [`sample_ack`], reads one envelope, and
/// sends a `BusEvent::Process` event BEFORE answering it — the daemon's own
/// documented ordering (`shep-daemon/tests/daemon_e2e.rs:161-174`): a
/// sheep's bus event can legitimately arrive ahead of the reply for the
/// very request that caused it.
///
/// Backed by a [`FakeDaemon`] (armed with a private `ScriptCommand::EventThenReply`);
/// see [`fake_client_replying_err`]'s own doc for why that matters.
pub async fn fake_client_event_then_reply(path: &Path) -> (Client, FakeDaemon) {
    let (client, daemon) = fake_client_on(path).await;
    daemon
        .script
        .send(ScriptCommand::EventThenReply)
        .await
        .unwrap();
    (client, daemon)
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

/// Binds `path`, handshakes with [`sample_ack`], reads exactly one
/// envelope, then drops the connection WITHOUT replying — for testing that
/// a request already accepted by the connection actor (and thus already
/// sitting in its `pending` map) fails with `RequestError::Closed` when the
/// connection dies mid-flight, rather than only covering the case where the
/// connection was already gone before the request was ever sent (that's
/// [`fake_client_that_closes_after_handshake`]).
pub async fn fake_client_that_dies_mid_request(path: &Path) -> (Client, JoinHandle<()>) {
    let listener = UnixListener::bind(path).unwrap();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut frames = Framed::new(stream, codec());
        handshake(&mut frames, sample_ack()).await;
        let _envelope = read_envelope(&mut frames).await;
        // Dropping `frames` here — after reading, not before — closes the
        // connection only once the actor has already written the request
        // and recorded it as pending.
    });
    let client = Client::connect(path).await.unwrap();
    (client, task)
}

/// Binds `path`, handshakes with [`sample_ack`], then reads nothing and
/// replies to nothing, ever — for testing a `Client`'s own client-side
/// deadline against a daemon that accepted the connection but stopped
/// answering.
///
/// Returns `(Client, JoinHandle<()>)`, not `(Client, FakeDaemon)`, even
/// though the phase 3 roster pins the latter
/// (`docs/writing-plans/plans/2026-08-08-shep-phase3-cli.md:291`).
/// `FakeDaemon`'s `serve_scripted` loop always answers SOME request
/// promptly (`ListFlock` per the armed script, everything else with
/// `Pong`) — there is no script command that means "never answer," so a
/// `FakeDaemon`-backed version of this helper could not do what its name
/// promises. Deliberate divergence from the roster, not an oversight.
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
