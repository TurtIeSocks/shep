//! Test doubles for a scripted daemon peer, shared with shep-cli via the
//! `test-support` feature. The only module that defines a `fake_daemon`.
//!
//! Every helper takes the socket path as `&Path`; the caller owns the
//! `TempDir`. No dev-dependencies, so `missing_docs` and
//! `missing_debug_implementations` apply here like any other public module.
//!
//! [`fake_daemon`], [`sample_ack`] and [`sample_info`] are handshake-only
//! primitives; [`FakeDaemon`] and the `fake_client_*` helpers connect a
//! real [`Client`] against a scripted peer; [`fast_opts`],
//! [`start_fake_daemon_answering_on`] and [`child_exiting_with`] serve the
//! autostart tests.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use shep_core::transport::{Listener, ServerStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::codec::Framed;

use shep_core::protocol::{
    BusEvent, DogSource, Envelope, ExitInfo, Hello, HelloAck, HelloReply, Lamb, PROTOCOL_VERSION,
    ProcessEventKind, ProcessInfo, Reply, Request, Response, RpcError, RpcErrorCode, codec,
    decode_frame, encode_frame,
};
use shep_core::status::ProcStatus;

use crate::{Client, ReconnectingClient};

/// A control address valid on the platform running the test, unique to
/// `dir`.
///
/// Unix uses `dir` directly. Windows names a pipe in a machine-global
/// namespace instead of a path under `dir`, matching
/// [`ShepPaths::pipe_name`](shep_core::paths::ShepPaths::pipe_name)'s own
/// derivation; the pid is folded in too, since each `cargo test` binary is
/// its own process and could otherwise collide on a shared `TempDir` name.
#[must_use]
pub fn control_address(dir: &Path) -> PathBuf {
    #[cfg(unix)]
    {
        dir.join("s.sock")
    }
    #[cfg(windows)]
    {
        let sanitized: String = dir
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        PathBuf::from(format!(
            r"\\.\pipe\shep-test-{}-{}",
            std::process::id(),
            sanitized.trim_matches('-')
        ))
    }
}

/// The framed transport a fake daemon holds for one accepted client.
///
/// Not [`crate::connection::Frames`], the client's side: the two coincide
/// on unix but differ on Windows, where a named pipe's server end is its
/// own type.
type Frames = Framed<ServerStream, tokio_util::codec::LengthDelimitedCodec>;
use crate::spawn::SpawnOptions;

/// Serves exactly one connection, replying to the `Hello` with `reply` and
/// closing. Returns the `Hello` the client actually sent.
///
/// Binds before returning, so a caller can `connect` immediately without a
/// sleep.
///
/// Panics if `path` cannot be bound or the connection fails partway
/// through the handshake.
pub async fn fake_daemon(path: &Path, reply: HelloReply) -> JoinHandle<Hello> {
    let mut listener = Listener::bind(path).unwrap();
    tokio::spawn(async move {
        let stream = listener.accept().await.unwrap();
        let mut frames = Framed::new(stream, codec());
        let first = frames.next().await.unwrap().unwrap();
        let hello: Hello = decode_frame(&first).unwrap();
        frames.send(encode_frame(&reply).unwrap()).await.unwrap();
        hello
    })
}

/// Binds `path`, accepts one connection, handshakes with `ack`, answers the
/// first request with `response`, and returns the received envelope.
///
/// Unlike [`fake_client_on`] and its siblings, this does not connect its own
/// [`Client`]: it only listens, for a caller (`shep-cli`'s `DogRuntime::start`)
/// that performs its own `Client::connect`.
///
/// Panics on any accept, handshake, decode or encode failure.
pub async fn serve_one_request(
    path: &Path,
    ack: HelloAck,
    response: Response,
) -> JoinHandle<Envelope> {
    let mut listener = Listener::bind(path).unwrap();
    tokio::spawn(async move {
        let stream = listener.accept().await.unwrap();
        let mut frames = Framed::new(stream, codec());
        let _hello = handshake(&mut frames, ack).await;
        let envelope = read_envelope(&mut frames).await;
        write_reply(&mut frames, envelope.id, response).await;
        envelope
    })
}

/// Binds `path` and answers every connection, one handshake and one request
/// each, with `reply`, until the returned handle is aborted.
///
/// The `served` counter is returned separately from the task: the accept
/// loop never ends on its own, so a `JoinHandle<u32>` could never be read
/// (`abort()` gives `JoinError::Cancelled`, an await waits forever). The
/// `AtomicU32` can be read while the fake is still running.
///
/// Panics if `path` cannot be bound.
pub fn fake_daemon_accepting_repeatedly(
    path: &Path,
    reply: Response,
) -> (JoinHandle<()>, Arc<AtomicU32>) {
    fake_daemon_accepting_repeatedly_with_ack(path, sample_ack(), reply)
}

/// As [`fake_daemon_accepting_repeatedly`], but with a caller-chosen
/// [`HelloAck`], for a caller whose version-skew guard would otherwise
/// refuse [`sample_ack`]'s fixed `"9.9.9"`.
///
/// Panics if `path` cannot be bound.
pub fn fake_daemon_accepting_repeatedly_with_ack(
    path: &Path,
    ack: HelloAck,
    reply: Response,
) -> (JoinHandle<()>, Arc<AtomicU32>) {
    let mut listener = Listener::bind(path).unwrap();
    let served = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&served);
    let handle = tokio::spawn(async move {
        while let Ok(stream) = listener.accept().await {
            let mut frames = Framed::new(stream, codec());
            let _hello = handshake(&mut frames, ack.clone()).await;
            let envelope = read_envelope(&mut frames).await;
            // `write_reply` already wraps the response in `Ok`.
            write_reply(&mut frames, envelope.id, reply.clone()).await;
            counter.fetch_add(1, Ordering::SeqCst);
        }
    });
    (handle, served)
}

/// What one generation of [`fake_daemon_across_handovers`] does with the
/// `Hello` a client sends it: the three shapes a reconnecting client has
/// to survive, and no others.
#[derive(Debug, Clone)]
pub enum Handshake {
    /// Answer the `Hello` with this ack, then serve requests until the
    /// connection is cut.
    Accept(HelloAck),
    /// Refuse the `Hello` with this error and close, as a successor
    /// compiled against an older protocol would.
    Refuse(RpcError),
    /// Close without answering the `Hello`, as a successor still coming up
    /// would.
    Drop,
}

/// A fake shepherd that survives a handover: one listener, one accepted
/// connection at a time, and a [`Self::cut`] that drops the accepted
/// connection out from under the client while the listener stays bound,
/// matching a real daemon's `execve`, which carries the listening socket
/// across but cannot carry an accepted connection.
///
/// Each connection is served by the next [`Handshake`] in the list; once
/// they run out, the last one repeats.
///
/// Panics if `path` cannot be bound, or on any accept, decode or encode
/// failure.
#[derive(Debug)]
pub struct Handovers {
    cut: mpsc::Sender<()>,
    cut_on_next_request: Arc<AtomicBool>,
    accepted: Arc<AtomicU32>,
    hellos: Arc<Mutex<Vec<Hello>>>,
    envelopes: Arc<Mutex<Vec<(u32, Envelope)>>>,
    armed_list: Arc<Mutex<Vec<ProcessInfo>>>,
    armed_dog_section: Arc<Mutex<String>>,
    task: JoinHandle<()>,
}

impl Handovers {
    /// Drops the currently accepted connection, leaving the listener bound,
    /// matching what a real `execve` produces for the client.
    pub async fn cut(&self) {
        let _ = self.cut.send(()).await;
    }

    /// Arms the current connection to read its next request envelope, then
    /// die without answering it.
    ///
    /// Synchronous, not `async`: an `await` here would give the serving
    /// task a chance to run before the test arms it.
    pub fn cut_on_next_request(&self) {
        self.cut_on_next_request.store(true, Ordering::SeqCst);
    }

    /// How many connections this fake has accepted so far, across every
    /// generation.
    #[must_use]
    pub fn accepted(&self) -> u32 {
        self.accepted.load(Ordering::SeqCst)
    }

    /// Every `Hello` this fake has read, in order, including ones it went
    /// on to refuse: the only path where a real daemon learns which dog it
    /// just turned away.
    #[must_use]
    pub fn hellos(&self) -> Vec<Hello> {
        self.hellos.lock().unwrap().clone()
    }

    /// Every request envelope received so far, paired with the 1-based
    /// generation that received it.
    #[must_use]
    pub fn envelopes(&self) -> Vec<(u32, Envelope)> {
        self.envelopes.lock().unwrap().clone()
    }

    /// Arms the answer every generation gives to `Request::ListFlock`.
    /// Unlike [`FakeDaemon::reply_to_list`], not consumed: a reload test
    /// asks both generations the same question.
    pub fn reply_to_list(&self, flock: Vec<ProcessInfo>) {
        *self.armed_list.lock().unwrap() = flock;
    }

    /// Arms the `[<name>]` section every generation answers
    /// `Request::DogConfig` with. Empty until set, matching a home with no
    /// dog configured.
    ///
    /// Not consumed, like [`Self::reply_to_list`]: a dog asks again after a
    /// handover.
    pub fn reply_to_dog_config(&self, section: &str) {
        *self.armed_dog_section.lock().unwrap() = section.to_owned();
    }
}

impl Drop for Handovers {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Binds `path` and serves one connection per entry in `handshakes`, in
/// order. See [`Handovers`] for what this fixture models.
///
/// Panics if `path` cannot be bound.
#[must_use]
pub fn fake_daemon_across_handovers(path: &Path, handshakes: Vec<Handshake>) -> Handovers {
    assert!(
        !handshakes.is_empty(),
        "a handover fixture needs at least one generation"
    );
    let mut listener = Listener::bind(path).unwrap();
    let (cut_tx, mut cut_rx) = mpsc::channel(SCRIPT_CHANNEL_CAPACITY);
    let cut_on_next_request = Arc::new(AtomicBool::new(false));
    let accepted = Arc::new(AtomicU32::new(0));
    let hellos: Arc<Mutex<Vec<Hello>>> = Arc::new(Mutex::new(Vec::new()));
    let envelopes: Arc<Mutex<Vec<(u32, Envelope)>>> = Arc::new(Mutex::new(Vec::new()));
    let armed_list: Arc<Mutex<Vec<ProcessInfo>>> = Arc::new(Mutex::new(Vec::new()));
    let armed_dog_section: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

    let task = tokio::spawn({
        let cut_on_next_request = Arc::clone(&cut_on_next_request);
        let accepted = Arc::clone(&accepted);
        let hellos = Arc::clone(&hellos);
        let envelopes = Arc::clone(&envelopes);
        let armed_list = Arc::clone(&armed_list);
        let armed_dog_section = Arc::clone(&armed_dog_section);
        async move {
            let mut generation: u32 = 0;
            while let Ok(stream) = listener.accept().await {
                generation += 1;
                accepted.fetch_add(1, Ordering::SeqCst);
                let index = usize::try_from(generation - 1).unwrap_or(usize::MAX);
                let script = handshakes
                    .get(index)
                    .unwrap_or_else(|| handshakes.last().expect("non-empty, asserted above"))
                    .clone();
                let mut frames = Framed::new(stream, codec());
                match script {
                    Handshake::Drop => continue,
                    Handshake::Refuse(err) => {
                        // Recorded before the refusal: the only path where
                        // a real daemon learns which dog it just refused.
                        if let Some(Ok(first)) = frames.next().await {
                            hellos.lock().unwrap().push(decode_frame(&first).unwrap());
                        }
                        let reply: HelloReply = Err(err);
                        let _ = frames.send(encode_frame(&reply).unwrap()).await;
                        continue;
                    }
                    Handshake::Accept(ack) => {
                        let hello = handshake(&mut frames, ack).await;
                        hellos.lock().unwrap().push(hello);
                    }
                }
                loop {
                    tokio::select! {
                        frame = frames.next() => {
                            let Some(Ok(bytes)) = frame else { break };
                            let envelope: Envelope = decode_frame(&bytes).unwrap();
                            let id = envelope.id;
                            let body = envelope.body.clone();
                            envelopes.lock().unwrap().push((generation, envelope));
                            if cut_on_next_request.swap(false, Ordering::SeqCst) {
                                break;
                            }
                            let response = match body {
                                Request::ListFlock => {
                                    Response::Flock(armed_list.lock().unwrap().clone())
                                }
                                Request::Subscribe { .. } => Response::Subscribed,
                                // A dog refuses to run on a reply it can't
                                // parse, and this is its first request.
                                Request::DogConfig { .. } => Response::DogSection {
                                    toml: armed_dog_section.lock().unwrap().clone().into(),
                                },
                                _ => Response::Pong,
                            };
                            write_reply(&mut frames, id, response).await;
                        }
                        _ = cut_rx.recv() => break,
                    }
                }
            }
        }
    });

    Handovers {
        cut: cut_tx,
        cut_on_next_request,
        accepted,
        hellos,
        envelopes,
        armed_list,
        armed_dog_section,
        task,
    }
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

/// One fully-populated [`ProcessInfo`]: every `Option` is `Some`, so an
/// anti-drift test sees every serialized field.
#[must_use]
pub fn sample_info() -> ProcessInfo {
    ProcessInfo::builder(1, "web", ProcStatus::Online)
        .pid(Some(4242))
        .restarts(3)
        .uptime_ms(60_000)
        .fold(Some("backend".to_string()))
        .out_file(Some("/home/ada/.shep/logs/web-0-out.log".to_string()))
        .err_file(Some("/home/ada/.shep/logs/web-0-err.log".to_string()))
        .cpu_percent(Some(12.5))
        .memory_bytes(Some(48 * 1024 * 1024))
        .dog(Some(DogSource::BuiltIn))
        .lambs(Some(vec![Lamb::new(4243, "node")]))
        // `restarts: 3` already implies an exit; give it a real outcome
        // rather than `None` so `last_exit`'s JSON shape gets exercised too.
        .last_exit(Some(ExitInfo {
            code: Some(1),
            signal: None,
        }))
        // Non-ASCII, like a real deploy dog's mark: every `Option` here
        // must be `Some`.
        .smit(Some("\u{25b2} main@a1b2c3".to_string()))
        .build()
}

/// Depth of a [`FakeDaemon`]'s script channel and of
/// [`fake_client_capturing_envelopes`]'s capture channel: generous and
/// untuned, like `shep-daemon`'s own `CHANNEL_CAPACITY`.
const SCRIPT_CHANNEL_CAPACITY: usize = 8;

/// Completes the handshake: reads the client's `Hello`, answers with
/// `ack`, and returns the `Hello`.
///
/// Panics on any accept, read, decode or write failure.
async fn handshake(frames: &mut Frames, ack: HelloAck) -> Hello {
    let first = frames.next().await.unwrap().unwrap();
    let hello: Hello = decode_frame(&first).unwrap();
    let reply: HelloReply = Ok(ack);
    frames.send(encode_frame(&reply).unwrap()).await.unwrap();
    hello
}

/// Reads and decodes the next envelope. Panics on failure or a closed
/// connection.
async fn read_envelope(frames: &mut Frames) -> Envelope {
    let frame = frames.next().await.unwrap().unwrap();
    decode_frame(&frame).unwrap()
}

/// Encodes and sends one successful [`Reply`] for `id`. Panics on failure.
async fn write_reply(frames: &mut Frames, id: u64, response: Response) {
    let reply = Reply {
        id,
        result: Ok(response),
    };
    frames.send(encode_frame(&reply).unwrap()).await.unwrap();
}

/// Encodes and sends one error [`Reply`] for `id`. Panics on failure.
async fn write_err(frames: &mut Frames, id: u64, code: RpcErrorCode, message: String) {
    let reply = Reply {
        id,
        result: Err(RpcError {
            code,
            message,
            daemon_version: None,
        }),
    };
    frames.send(encode_frame(&reply).unwrap()).await.unwrap();
}

/// Encodes and sends one [`BusEvent`] frame directly, not wrapped in a
/// [`Reply`]: the shape a real subscriber receives. Panics on failure.
async fn write_event(frames: &mut Frames, event: BusEvent) {
    frames.send(encode_frame(&event).unwrap()).await.unwrap();
}

/// Sends a `BusEvent::Process` built from [`sample_info`]: a sheep's bus
/// event can legitimately arrive ahead of the reply for the request that
/// caused it. Panics on failure.
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
/// go through a `Mutex`-backed flag instead, since every call site invokes
/// them without `.await`. This enum carries the rest, each armed at most
/// once.
enum ScriptCommand {
    /// Arms the next request to receive an [`RpcError`] with this `code`
    /// and `message` instead of a normal response.
    ReplyErr(RpcErrorCode, String),
    /// Arms the next request to receive a [`sample_info`]-based
    /// `BusEvent::Process` before its `Pong` reply.
    EventThenReply,
    /// Buffers the next two requests, then answers the `ListFlock` one
    /// first and the other second, regardless of arrival order.
    ArmOutOfOrder,
    /// Queues one [`BusEvent`] for this connection's subscriber, buffered
    /// until the connection has answered a `Request::Subscribe`.
    ///
    /// Boxed: `BusEvent::Process` carries a `ProcessInfo`, which is past
    /// clippy's `large_enum_variant` threshold; every other variant here is
    /// a few bytes.
    PushEvent(Box<BusEvent>),
    /// Ends the script: stop serving and let the task return.
    Close,
    /// Ends the script once this connection has answered its next
    /// `Request::Subscribe` and flushed anything queued via
    /// [`FakeDaemon::push`].
    CloseAfterSubscribe,
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

/// A scripted daemon over one accepted connection. Every request not
/// covered by an armed method answers `Response::Pong`.
///
/// A few `fake_client_*` constructors below arm a one-shot error, an
/// out-of-order reply, or an event-before-reply via a private
/// `ScriptCommand`, since nothing outside this module needs to arm those
/// directly.
#[derive(Debug)]
pub struct FakeDaemon {
    script: mpsc::Sender<ScriptCommand>,
    armed_list: Arc<Mutex<Option<Vec<ProcessInfo>>>>,
    armed_list_sequence: Arc<Mutex<VecDeque<Vec<ProcessInfo>>>>,
    armed_describe: Arc<Mutex<Option<Vec<ProcessInfo>>>>,
    armed_reply_then_event: Arc<Mutex<Option<(Response, BusEvent)>>>,
    armed_shutdown_then_unlink: Arc<Mutex<Option<Duration>>>,
    armed_shutdown_never_unlink: Arc<Mutex<Option<()>>>,
    list_flock_count: Arc<AtomicU64>,
    task: JoinHandle<()>,
}

impl FakeDaemon {
    /// Arms the answer to the next `Request::ListFlock` this connection
    /// receives.
    ///
    /// Synchronous: an `async fn` here would trip `unused_must_use` since
    /// every call site invokes it without `.await`.
    pub fn reply_to_list(&self, flock: Vec<ProcessInfo>) {
        *self.armed_list.lock().unwrap() = Some(flock);
    }

    /// Arms a whole sequence of `Request::ListFlock` answers at once: the
    /// first call gets `responses[0]`, the second `responses[1]`, and so on.
    /// Once the queue empties, [`Self::reply_to_list`]'s single-slot arming
    /// takes back over.
    ///
    /// Synchronous, like [`Self::reply_to_list`].
    pub fn reply_to_list_sequence(&self, responses: Vec<Vec<ProcessInfo>>) {
        *self.armed_list_sequence.lock().unwrap() = responses.into();
    }

    /// Arms the answer to the next `Request::Describe { .. }` this
    /// connection receives, regardless of the selector inside it.
    ///
    /// Synchronous, like [`Self::reply_to_list`].
    pub fn reply_to_describe(&self, procs: Vec<ProcessInfo>) {
        *self.armed_describe.lock().unwrap() = Some(procs);
    }

    /// How many `Request::ListFlock` envelopes this connection has
    /// answered so far.
    #[must_use]
    pub fn list_flock_count(&self) -> u64 {
        self.list_flock_count.load(Ordering::SeqCst)
    }

    /// Arms the reply this connection sends for its next request, then
    /// immediately follows it with `event` written directly to the wire:
    /// matches the ordering a real subscribe produces, reply ahead of any
    /// event.
    ///
    /// Synchronous, like [`Self::reply_to_list`].
    pub fn queue_reply_then_event(&self, reply: Response, event: BusEvent) {
        *self.armed_reply_then_event.lock().unwrap() = Some((reply, event));
    }

    /// Arms the next request to be answered `Response::ShuttingDown`; this
    /// connection then waits `after` and unlinks its socket file.
    ///
    /// Synchronous, like [`Self::reply_to_list`].
    pub fn reply_shutting_down_then_unlink_after(&self, after: Duration) {
        *self.armed_shutdown_then_unlink.lock().unwrap() = Some(after);
    }

    /// Arms the next request to be answered `Response::ShuttingDown` and
    /// then nothing: the socket file stays.
    ///
    /// Synchronous, like [`Self::reply_to_list`].
    pub fn reply_shutting_down_and_never_unlink(&self) {
        *self.armed_shutdown_never_unlink.lock().unwrap() = Some(());
    }

    /// Queues `event` for this connection's subscriber.
    ///
    /// Buffered in arrival order until the connection has answered a
    /// `Request::Subscribe`, since a `broadcast::Receiver` never sees a
    /// value sent before it existed. Written straight to the wire once
    /// subscribed.
    ///
    /// Silently does nothing if the background task has already ended.
    pub async fn push(&self, event: BusEvent) {
        let _ = self
            .script
            .send(ScriptCommand::PushEvent(Box::new(event)))
            .await;
    }

    /// Pushes `EVENT_CHANNEL_CAPACITY + n` [`BusEvent::LogOut`] events in
    /// one go, enough to force a local lag notice instead of ordinary
    /// delivery.
    pub async fn overrun_by(&self, n: usize) {
        for i in 0..(crate::actor::EVENT_CHANNEL_CAPACITY + n) {
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
    /// Panics if the background task is gone or panicked.
    pub async fn close(self) {
        let _ = self.script.send(ScriptCommand::Close).await;
        self.task.await.unwrap();
    }

    /// Arms this connection to close itself once it has answered its next
    /// `Request::Subscribe` and flushed anything [`Self::push`] queued
    /// beforehand, unlike calling [`Self::close`] before the subscription
    /// exists.
    ///
    /// Goes through the script channel rather than a `Mutex` flag: it arms
    /// behavior on the same background task that drains [`Self::push`]'s
    /// queue.
    ///
    /// Does not consume `self`, unlike [`Self::close`]: a caller may still
    /// want [`Self::list_flock_count`] afterward.
    pub async fn close_after_subscribe(&self) {
        let _ = self.script.send(ScriptCommand::CloseAfterSubscribe).await;
    }
}

/// The [`FakeDaemon`] background task: accepts one connection, handshakes
/// with `ack`, then answers requests until [`ScriptCommand::Close`] arrives
/// or the connection ends.
///
/// Checked in priority order per request: a buffered out-of-order request,
/// then an armed error, event or shutdown script, then `Subscribe`, then
/// `ListFlock`/`Describe`, falling back to `Response::Pong`.
///
/// One `Arc<Mutex<..>>` slot per independently armed behavior, cloned from
/// [`FakeDaemon`]'s own fields. Eleven parameters: bundling them into a
/// struct would move the coupling around, not reduce it, for this
/// private, one-caller function.
#[allow(clippy::too_many_arguments)]
async fn serve_scripted(
    mut listener: Listener,
    socket_path: PathBuf,
    ack: HelloAck,
    mut script: mpsc::Receiver<ScriptCommand>,
    armed_list: Arc<Mutex<Option<Vec<ProcessInfo>>>>,
    armed_list_sequence: Arc<Mutex<VecDeque<Vec<ProcessInfo>>>>,
    armed_describe: Arc<Mutex<Option<Vec<ProcessInfo>>>>,
    armed_reply_then_event: Arc<Mutex<Option<(Response, BusEvent)>>>,
    armed_shutdown_then_unlink: Arc<Mutex<Option<Duration>>>,
    armed_shutdown_never_unlink: Arc<Mutex<Option<()>>>,
    list_flock_count: Arc<AtomicU64>,
) {
    let stream = listener.accept().await.unwrap();
    let mut frames = Framed::new(stream, codec());
    let _hello = handshake(&mut frames, ack).await;

    let mut armed_err: Option<(RpcErrorCode, String)> = None;
    let mut armed_event_then_reply = false;
    let mut out_of_order = OutOfOrder::Idle;
    // Before `Subscribe` is answered, a pushed event queues here: the
    // client's `broadcast::Receiver` does not exist yet.
    let mut subscribed = false;
    let mut pending_events: Vec<BusEvent> = Vec::new();
    let mut close_after_subscribe = false;

    loop {
        tokio::select! {
            command = script.recv() => {
                match command {
                    Some(ScriptCommand::ReplyErr(code, message)) => armed_err = Some((code, message)),
                    Some(ScriptCommand::EventThenReply) => armed_event_then_reply = true,
                    Some(ScriptCommand::ArmOutOfOrder) => out_of_order = OutOfOrder::Armed,
                    Some(ScriptCommand::PushEvent(event)) => {
                        if subscribed {
                            write_event(&mut frames, *event).await;
                        } else {
                            pending_events.push(*event);
                        }
                    }
                    Some(ScriptCommand::CloseAfterSubscribe) => {
                        close_after_subscribe = true;
                        // `select!`'s arms are unbiased: `Subscribe` may
                        // already be handled, so re-check and close now if
                        // it already happened.
                        if subscribed {
                            break;
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
                        // Taken into owned locals before any `.await`:
                        // `MutexGuard` is not `Send`, and `tokio::spawn`
                        // requires the future to be.
                        let reply_then_event = armed_reply_then_event.lock().unwrap().take();
                        let shutdown_then_unlink =
                            armed_shutdown_then_unlink.lock().unwrap().take();
                        let shutdown_never_unlink =
                            armed_shutdown_never_unlink.lock().unwrap().take();
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
                        } else if let Some(after) = shutdown_then_unlink {
                            write_reply(&mut frames, envelope.id, Response::ShuttingDown).await;
                            // Run inline: `kill` drops the `Client` right
                            // after this reply, closing the connection
                            // before a deferred unlink would get a turn.
                            tokio::time::sleep(after).await;
                            let _ = std::fs::remove_file(&socket_path);
                        } else if shutdown_never_unlink.is_some() {
                            write_reply(&mut frames, envelope.id, Response::ShuttingDown).await;
                            // Never unlinks: the branch a kill-teardown
                            // timeout exists to observe.
                        } else if matches!(envelope.body, Request::Subscribe { .. }) {
                            write_reply(&mut frames, envelope.id, Response::Subscribed).await;
                            subscribed = true;
                            for event in pending_events.drain(..) {
                                write_event(&mut frames, event).await;
                            }
                            if close_after_subscribe {
                                break;
                            }
                        } else {
                            let response = if matches!(envelope.body, Request::ListFlock) {
                                list_flock_count.fetch_add(1, Ordering::SeqCst);
                                // The sequence queue takes priority over a
                                // single `reply_to_list` slot armed at the
                                // same time.
                                let next = armed_list_sequence.lock().unwrap().pop_front();
                                Response::Flock(next.unwrap_or_else(|| {
                                    armed_list.lock().unwrap().take().unwrap_or_default()
                                }))
                            } else if matches!(envelope.body, Request::Describe { .. }) {
                                Response::Described(
                                    armed_describe.lock().unwrap().take().unwrap_or_default(),
                                )
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
/// [`Client`] alongside the still-live [`FakeDaemon`] script, for a test
/// that needs nothing daemon-specific.
pub async fn fake_client_on(path: &Path) -> (Client, FakeDaemon) {
    fake_client_with_ack(path, sample_ack()).await
}

/// As [`fake_client_on`], but with a caller-chosen [`HelloAck`], for a
/// test asserting on the ack a `Client` receives.
pub async fn fake_client_with_ack(path: &Path, ack: HelloAck) -> (Client, FakeDaemon) {
    let daemon = fake_daemon_scripted_on(path, ack);
    let client = Client::connect(path).await.unwrap();
    (client, daemon)
}

/// As [`fake_client_on`], but hands back a [`ReconnectingClient`], the
/// supervised wrapper a dog uses, instead of a bare [`Client`].
///
/// The [`FakeDaemon`] behind it still serves exactly one connection, so a
/// test that cuts it leaves the supervisor retrying against a gone
/// listener. Use [`fake_daemon_across_handovers`] to test the reconnect
/// itself.
pub async fn fake_reconnecting_client_on(path: &Path) -> (ReconnectingClient, FakeDaemon) {
    let daemon = fake_daemon_scripted_on(path, sample_ack());
    let client = ReconnectingClient::connect(path).await.unwrap();
    (client, daemon)
}

/// Binds `path` and starts the scripted fake without connecting a client
/// of its own, for a caller that performs its own connect.
///
/// Synchronous: the listener is bound before this returns, so a caller can
/// connect straight away without a sleep.
///
/// Panics if `path` cannot be bound.
#[must_use]
pub fn fake_daemon_scripted_on(path: &Path, ack: HelloAck) -> FakeDaemon {
    let listener = Listener::bind(path).unwrap();
    let (script_tx, script_rx) = mpsc::channel(SCRIPT_CHANNEL_CAPACITY);
    let armed_list = Arc::new(Mutex::new(None));
    let armed_list_sequence = Arc::new(Mutex::new(VecDeque::new()));
    let armed_describe = Arc::new(Mutex::new(None));
    let armed_reply_then_event = Arc::new(Mutex::new(None));
    let armed_shutdown_then_unlink = Arc::new(Mutex::new(None));
    let armed_shutdown_never_unlink = Arc::new(Mutex::new(None));
    let list_flock_count = Arc::new(AtomicU64::new(0));
    let task = tokio::spawn(serve_scripted(
        listener,
        path.to_path_buf(),
        ack,
        script_rx,
        Arc::clone(&armed_list),
        Arc::clone(&armed_list_sequence),
        Arc::clone(&armed_describe),
        Arc::clone(&armed_reply_then_event),
        Arc::clone(&armed_shutdown_then_unlink),
        Arc::clone(&armed_shutdown_never_unlink),
        Arc::clone(&list_flock_count),
    ));
    FakeDaemon {
        script: script_tx,
        armed_list,
        armed_list_sequence,
        armed_describe,
        armed_reply_then_event,
        armed_shutdown_then_unlink,
        armed_shutdown_never_unlink,
        list_flock_count,
        task,
    }
}

/// Identical to [`fake_client_on`], named separately for tests whose whole
/// point is [`FakeDaemon::push`], [`FakeDaemon::overrun_by`] or
/// [`FakeDaemon::queue_reply_then_event`].
pub async fn fake_client_with_push(path: &Path) -> (Client, FakeDaemon) {
    fake_client_on(path).await
}

/// Binds `path` and serves every connection made to it, handshaking with
/// `ack`, answering each request through `answer`, and forwarding each
/// decoded [`Envelope`] onto the returned channel.
///
/// Unlike [`fake_client_answering`], which accepts exactly one connection:
/// a test driving a whole CLI verb has the verb open its own connections,
/// and a second connect against a one-shot fake would sit in the kernel's
/// backlog. `ack` is a parameter so a test can vary
/// [`HelloAck::daemon_version`] rather than being pinned to [`sample_ack`].
///
/// The task is detached and runs until the listener errors, when the
/// caller's `TempDir` goes away.
pub async fn fake_daemon_answering_with_ack(
    path: &Path,
    ack: HelloAck,
    answer: impl Fn(&Request) -> Response + Send + Sync + Clone + 'static,
) -> mpsc::UnboundedReceiver<Envelope> {
    let mut listener = Listener::bind(path).unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok(stream) = listener.accept().await {
            let tx = tx.clone();
            let ack = ack.clone();
            let answer = answer.clone();
            tokio::spawn(async move {
                let mut frames = Framed::new(stream, codec());
                let _hello = handshake(&mut frames, ack).await;
                while let Some(Ok(frame)) = frames.next().await {
                    let Ok(envelope) = decode_frame::<Envelope>(&frame) else {
                        break;
                    };
                    let id = envelope.id;
                    let reply = answer(&envelope.body);
                    if tx.send(envelope).is_err() {
                        break;
                    }
                    write_reply(&mut frames, id, reply).await;
                }
            });
        }
    });
    rx
}

/// As [`fake_client_capturing_envelopes`], but `answer` decides each reply,
/// for a test asserting on what a multi-request caller puts on the wire.
/// `answer` is called with each decoded [`Request`] in arrival order and may
/// close over a counter to vary its reply. The envelope reaches the channel
/// before the reply is written, matching [`fake_client_capturing_envelopes`].
///
/// Unbounded, unlike every other channel in this module: a test reads these
/// envelopes only after the call under test returns, so a bounded channel
/// would block once `SCRIPT_CHANNEL_CAPACITY` requests are in flight.
///
/// The read loop ends on a closed or unreadable connection rather than
/// panicking, since a dropped `Client` here is an ordinary end, not a fault.
pub async fn fake_client_answering(
    path: &Path,
    answer: impl Fn(&Request) -> Response + Send + 'static,
) -> (Client, mpsc::UnboundedReceiver<Envelope>) {
    let mut listener = Listener::bind(path).unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let stream = listener.accept().await.unwrap();
        let mut frames = Framed::new(stream, codec());
        let _hello = handshake(&mut frames, sample_ack()).await;
        while let Some(Ok(frame)) = frames.next().await {
            let Ok(envelope) = decode_frame::<Envelope>(&frame) else {
                break;
            };
            let id = envelope.id;
            let reply = answer(&envelope.body);
            if tx.send(envelope).is_err() {
                break;
            }
            write_reply(&mut frames, id, reply).await;
        }
    });
    let client = Client::connect(path).await.unwrap();
    (client, rx)
}

/// Binds `path`, handshakes with [`sample_ack`], and answers every request
/// with `Response::Pong` while forwarding each decoded [`Envelope`] onto
/// the returned channel, for asserting on what a `Client` puts on the wire
/// rather than on how the daemon answers.
pub async fn fake_client_capturing_envelopes(path: &Path) -> (Client, mpsc::Receiver<Envelope>) {
    let mut listener = Listener::bind(path).unwrap();
    let (tx, rx) = mpsc::channel(SCRIPT_CHANNEL_CAPACITY);
    tokio::spawn(async move {
        let stream = listener.accept().await.unwrap();
        let mut frames = Framed::new(stream, codec());
        let _hello = handshake(&mut frames, sample_ack()).await;
        loop {
            let envelope = read_envelope(&mut frames).await;
            let id = envelope.id;
            // Forwarded before the reply is sent, so an awaited
            // `Client::request` future finds its envelope already queued.
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
/// request that arrives with an [`RpcError`] carrying `code` and `message`.
///
/// Backed by a [`FakeDaemon`] so the connection keeps serving afterward,
/// unlike a bespoke one-shot task that would die after the scripted reply.
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
/// envelopes, then answers the `ListFlock` one first and the `Ping` one
/// second, regardless of arrival order: proof that a `Client` routes
/// replies by id.
///
/// Backed by a [`FakeDaemon`], like [`fake_client_replying_err`].
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
/// sends a `BusEvent::Process` event before answering it: a sheep's bus
/// event can legitimately arrive ahead of the reply for the request that
/// caused it.
///
/// Backed by a [`FakeDaemon`], like [`fake_client_replying_err`].
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
/// connection, for testing that a `Client` fails every pending request with
/// `RequestError::Closed` rather than hanging.
pub async fn fake_client_that_closes_after_handshake(path: &Path) -> (Client, JoinHandle<()>) {
    let mut listener = Listener::bind(path).unwrap();
    let task = tokio::spawn(async move {
        let stream = listener.accept().await.unwrap();
        let mut frames = Framed::new(stream, codec());
        let _hello = handshake(&mut frames, sample_ack()).await;
        // Dropping `frames` here closes the connection from this side.
    });
    let client = Client::connect(path).await.unwrap();
    (client, task)
}

/// Binds `path`, handshakes with [`sample_ack`], reads exactly one
/// envelope, then drops the connection without replying: for testing that
/// a request already accepted into the connection actor's `pending` map
/// fails with `RequestError::Closed` when the connection dies mid-flight.
/// Unlike [`fake_client_that_closes_after_handshake`], which never accepts
/// the request at all.
pub async fn fake_client_that_dies_mid_request(path: &Path) -> (Client, JoinHandle<()>) {
    let mut listener = Listener::bind(path).unwrap();
    let task = tokio::spawn(async move {
        let stream = listener.accept().await.unwrap();
        let mut frames = Framed::new(stream, codec());
        let _hello = handshake(&mut frames, sample_ack()).await;
        let _envelope = read_envelope(&mut frames).await;
        // Dropping `frames` here, after reading, closes the connection
        // only once the actor has recorded the request as pending.
    });
    let client = Client::connect(path).await.unwrap();
    (client, task)
}

/// Binds `path`, handshakes with [`sample_ack`], then reads nothing and
/// replies to nothing, ever: for testing a `Client`'s own client-side
/// deadline against a daemon that accepted the connection but stopped
/// answering.
///
/// Returns `(Client, JoinHandle<()>)`, not `(Client, FakeDaemon)`:
/// `FakeDaemon`'s `serve_scripted` loop always answers some request
/// promptly, and no `ScriptCommand` means never answering.
pub async fn fake_client_that_never_replies(path: &Path) -> (Client, JoinHandle<()>) {
    let mut listener = Listener::bind(path).unwrap();
    let task = tokio::spawn(async move {
        let stream = listener.accept().await.unwrap();
        let mut frames = Framed::new(stream, codec());
        let _hello = handshake(&mut frames, sample_ack()).await;
        core::future::pending::<()>().await;
    });
    let client = Client::connect(path).await.unwrap();
    (client, task)
}

/// [`SpawnOptions`] tuned so `spawn.rs`'s tests finish in under a second on
/// a real clock, since none of them may pause tokio's clock.
///
/// `a_child_that_dies_fails_fast_instead_of_waiting_out_the_deadline` uses
/// the production defaults instead: it asserts on the 30s deadline itself.
#[must_use]
pub fn fast_opts() -> SpawnOptions {
    SpawnOptions {
        deadline: Duration::from_millis(600),
        backoff_start: Duration::from_millis(10),
        backoff_cap: Duration::from_millis(50),
        handshake_timeout: Duration::from_millis(100),
    }
}

/// Binds `path`, accepts one connection, answers its handshake with
/// [`sample_ack`], then parks, standing in for a daemon a launcher closure
/// needs to bring into existence synchronously.
///
/// Synchronous: a `connect_or_spawn` launcher is a plain
/// `FnOnce() -> io::Result<Child>`. Its `tokio::spawn` call still gets a
/// runtime context, since `connect_or_spawn_with` runs the launcher on
/// `spawn_blocking`'s pool. The returned task is detached and outlives
/// this call.
///
/// Panics if `path` cannot be bound.
pub fn start_fake_daemon_answering_on(path: &Path) {
    let mut listener = Listener::bind(path).unwrap();
    tokio::spawn(async move {
        let stream = listener.accept().await.unwrap();
        let mut frames = Framed::new(stream, codec());
        let _hello = handshake(&mut frames, sample_ack()).await;
        core::future::pending::<()>().await;
    });
}

/// A launcher-ready child that is already exiting with `code`: spawns
/// `sh -c "exit <code>"` and returns the `Child` immediately.
///
/// # Errors
///
/// Whatever `std::process::Command::spawn` can return, propagated rather
/// than unwrapped so this fits `connect_or_spawn`'s launcher signature.
pub fn child_exiting_with(code: i32) -> std::io::Result<std::process::Child> {
    std::process::Command::new("sh")
        .args(["-c", &format!("exit {code}")])
        .spawn()
}
