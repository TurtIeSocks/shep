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
//! [`fast_opts`], [`start_fake_daemon_answering_on`] and
//! [`child_exiting_with`] serve the autostart tests, where the peer has to
//! be brought into existence from a synchronous launcher closure.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
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

use crate::Client;

/// A control address valid on the platform running the test, unique to
/// `dir`.
///
/// Every fake in this module takes an address as `&Path`, and until the
/// Windows tier existed a caller could simply write `dir.path().join(
/// "s.sock")`. That still works on unix and cannot work on Windows, where
/// the control transport is a named pipe: `\\.\pipe\...` is a name in a
/// machine-global kernel namespace, not a path under a directory, so a
/// tempdir path names nothing a pipe can be created on.
///
/// The uniqueness argument is the reason this takes a `dir` it does not
/// otherwise need on Windows. The pipe namespace is shared by every process
/// on the machine, so two tests that picked one fixed name would contend for
/// real — passing individually and failing under `cargo test`'s default
/// parallelism, which is the worst way for this to show up. A `TempDir`'s
/// path is already unique per test, so folding it into the pipe name inherits
/// that uniqueness instead of inventing a second scheme. This is the same
/// derivation [`ShepPaths::pipe_name`](shep_core::paths::ShepPaths::pipe_name)
/// performs for a real `$SHEP_HOME`, which is why the two agree by
/// construction rather than by being kept in step.
///
/// The process id is folded in as well, because `cargo test` runs each
/// integration binary as its own process and two of them can hold
/// same-named tempdirs on some platforms.
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
/// Deliberately NOT [`crate::connection::Frames`], which is the *client's*
/// side. The two are the same type on unix, where a socket's two ends are
/// indistinguishable, and different types on Windows, where a named pipe's
/// server end is its own type. Every fake in this module is a server, so it
/// frames a [`ServerStream`]; reusing the client alias compiled on unix and
/// would not on Windows, which is precisely the bug this separate name
/// exists to make impossible.
type Frames = Framed<ServerStream, tokio_util::codec::LengthDelimitedCodec>;
use crate::spawn::SpawnOptions;

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
/// first request it receives with `response`, and returns the envelope it
/// received.
///
/// Unlike [`fake_client_on`] and its siblings below, this does not connect a
/// [`Client`] of its own — it only listens, so the caller's OWN connect is
/// the first and only thing that ever dials it. That is the shape
/// `shep-cli`'s `DogRuntime::start` needs to be tested against: it performs
/// its own `Client::connect`, so a fixture that already holds a connected
/// `Client` (every `fake_client_*` helper below) cannot stand in for the
/// peer on the other end of it.
///
/// Panics on any accept/handshake/decode/encode failure, same as
/// [`fake_daemon`] — test scaffolding, meant to fail the test loudly rather
/// than surface a `Result` nobody would check.
pub async fn serve_one_request(
    path: &Path,
    ack: HelloAck,
    response: Response,
) -> JoinHandle<Envelope> {
    let mut listener = Listener::bind(path).unwrap();
    tokio::spawn(async move {
        let stream = listener.accept().await.unwrap();
        let mut frames = Framed::new(stream, codec());
        handshake(&mut frames, ack).await;
        let envelope = read_envelope(&mut frames).await;
        write_reply(&mut frames, envelope.id, response).await;
        envelope
    })
}

/// Binds `path` and answers EVERY connection — one handshake and one request
/// each — with `reply`, until the returned handle is aborted.
///
/// Every other fake in this module accepts exactly one connection
/// (`serve_scripted` opens with a bare `accept` and then loops over frames),
/// which is right for a caller handed an already-connected [`Client`]. `shep
/// whistle` is the first caller in this workspace that opens a fresh
/// connection per request — see `shep-cli/src/whistle/shepherd.rs` for why —
/// so it needs a listener that outlives the first call.
///
/// The returned `served` counter is shared, not the task's return value: the
/// accept loop never ends on its own, so a `JoinHandle<u32>` would carry a
/// number no caller could ever read (a caller that `abort()`s gets
/// `JoinError::Cancelled`, and a caller that awaits waits forever). An
/// `AtomicU32` the test reads WHILE the fake is still running is what lets a
/// test assert that a request was made exactly once rather than retried.
///
/// Panics if `path` cannot be bound — test scaffolding, the same failure mode
/// [`fake_daemon`] documents.
pub fn fake_daemon_accepting_repeatedly(
    path: &Path,
    reply: Response,
) -> (JoinHandle<()>, Arc<AtomicU32>) {
    let mut listener = Listener::bind(path).unwrap();
    let served = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&served);
    let handle = tokio::spawn(async move {
        while let Ok(stream) = listener.accept().await {
            let mut frames = Framed::new(stream, codec());
            handshake(&mut frames, sample_ack()).await;
            let envelope = read_envelope(&mut frames).await;
            // `write_reply` wraps the value in `Ok` itself — its signature is
            // `(&mut Frames, u64, Response)`, testing.rs:155 — so passing an
            // `Ok(...)` here is a type error, not a courtesy.
            write_reply(&mut frames, envelope.id, reply.clone()).await;
            counter.fetch_add(1, Ordering::SeqCst);
        }
    });
    (handle, served)
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
    ProcessInfo::builder(1, "web", ProcStatus::Online)
        .pid(Some(4242))
        .restarts(3)
        .uptime_ms(60_000)
        .fold(Some("backend".to_string()))
        .out_file(Some("/home/rin/.shep/logs/web-0-out.log".to_string()))
        .err_file(Some("/home/rin/.shep/logs/web-0-err.log".to_string()))
        .cpu_percent(Some(12.5))
        .memory_bytes(Some(48 * 1024 * 1024))
        .dog(Some(DogSource::BuiltIn))
        .lambs(Some(vec![Lamb::new(4243, "node")]))
        // `restarts: 3` above already says this sheep has exited before;
        // giving it a real outcome here rather than `None` keeps that
        // consistent, and keeps this fixture exercising `last_exit`'s own
        // JSON shape the same way it exercises every other field's.
        .last_exit(Some(ExitInfo {
            code: Some(1),
            signal: None,
        }))
        .build()
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
    ///
    /// Boxed (task 49): `BusEvent::Process` carries a `ProcessInfo`, which
    /// grew past clippy's `large_enum_variant` threshold the moment
    /// `last_exit` landed on it. Every other variant here is a handful of
    /// bytes, so boxing this one rather than allowing the lint keeps the
    /// enum's stack footprint tied to its smallest common case, not its
    /// largest.
    PushEvent(Box<BusEvent>),
    /// Ends the script: stop serving and let the task return.
    Close,
    /// Ends the script the moment this connection has answered its next
    /// `Request::Subscribe` and flushed anything already queued via
    /// [`FakeDaemon::push`] — see [`FakeDaemon::close_after_subscribe`].
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

/// A scripted daemon over one accepted connection.
///
/// [`Self::reply_to_list`] arms the answer to the next `Request::ListFlock`;
/// [`Self::reply_to_describe`] arms the answer to the next
/// `Request::Describe { .. }` (the shape both `describe` and `fold` send);
/// [`Self::list_flock_count`] reports how many `ListFlock` requests this
/// connection has answered, so a test can prove a client cached rather than
/// re-asked; [`Self::close`] ends the script and drops the connection;
/// [`Self::close_after_subscribe`] does the same but only once this
/// connection's next `Request::Subscribe` has actually been answered,
/// which is what a test needs when the script has to queue `close`
/// *before* the client under test has connected at all.
/// [`Self::reply_shutting_down_then_unlink_after`] arms the next request to
/// be answered `Response::ShuttingDown` and, after a delay, unlinks this
/// connection's socket file — the real teardown sequence, compressed;
/// [`Self::reply_shutting_down_and_never_unlink`] answers the same way but
/// leaves the socket file in place, the branch a kill-teardown timeout
/// exists to observe.
/// Every other request this fake receives is answered with
/// `Response::Pong` — good enough for a test that only cares about
/// `ListFlock`/`Describe` behavior, or about a request getting *some*
/// prompt reply.
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
    /// Synchronous, not `async`: every plan call site invokes this without
    /// `.await` (`docs/writing-plans/plans/2026-08-08-shep-phase3-cli.md`,
    /// e.g. lines 2425, 2452, 2475), which would trip `unused_must_use`
    /// under `-D warnings` against an `async fn`. A `Mutex`-backed flag
    /// lets this stay a plain fn instead.
    pub fn reply_to_list(&self, flock: Vec<ProcessInfo>) {
        *self.armed_list.lock().unwrap() = Some(flock);
    }

    /// Arms a whole sequence of `Request::ListFlock` answers at once: the
    /// first call gets `responses[0]`, the second `responses[1]`, and so on.
    ///
    /// For a test proving a caller re-asks on every scrape rather than
    /// caching the first reading — the shep-cli metrics dog's own
    /// `every_scrape_asks_the_shepherd_again` test. [`Self::reply_to_list`]
    /// only arms ONE answer at a time (consumed on the next `ListFlock`), so
    /// a test with two scrapes that must see two DIFFERENT listings would
    /// otherwise need to re-arm between them with no hook to do it from —
    /// the caller does not control when its own second request lands on the
    /// wire. Once this queue empties, [`Self::reply_to_list`]'s own
    /// single-slot arming (or the `Flock(vec![])` default) takes back over.
    ///
    /// Synchronous for the same reason [`Self::reply_to_list`] is: every
    /// call site invokes it without `.await`.
    pub fn reply_to_list_sequence(&self, responses: Vec<Vec<ProcessInfo>>) {
        *self.armed_list_sequence.lock().unwrap() = responses.into();
    }

    /// Arms the answer to the next `Request::Describe { .. }` this
    /// connection receives, regardless of the selector inside it —
    /// `describe` and `fold` both send this request shape, and this fake
    /// scripts the reply the same way for either.
    ///
    /// Synchronous for the same reason [`Self::reply_to_list`] is: every
    /// call site invokes it without `.await`.
    pub fn reply_to_describe(&self, procs: Vec<ProcessInfo>) {
        *self.armed_describe.lock().unwrap() = Some(procs);
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

    /// Arms the next request to be answered `Response::ShuttingDown`, after
    /// which this connection waits `after` and then unlinks its socket file
    /// — the real teardown sequence, compressed.
    ///
    /// Synchronous for the same reason [`Self::reply_to_list`] is: every
    /// call site invokes it without `.await`.
    pub fn reply_shutting_down_then_unlink_after(&self, after: Duration) {
        *self.armed_shutdown_then_unlink.lock().unwrap() = Some(after);
    }

    /// Arms the next request to be answered `Response::ShuttingDown` and then
    /// nothing: the socket file stays. The branch `kill`'s timeout exists
    /// for.
    ///
    /// Synchronous for the same reason [`Self::reply_to_list`] is: every
    /// call site invokes it without `.await`.
    pub fn reply_shutting_down_and_never_unlink(&self) {
        *self.armed_shutdown_never_unlink.lock().unwrap() = Some(());
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
        let _ = self
            .script
            .send(ScriptCommand::PushEvent(Box::new(event)))
            .await;
    }

    /// Pushes `EVENT_CHANNEL_CAPACITY + n` [`BusEvent::LogOut`] events in
    /// one go — enough, once a subscriber falls that far behind, to force a
    /// local lag notice rather than an ordinary delivery. `EVENT_CHANNEL_CAPACITY`
    /// is the actor's own broadcast channel capacity
    /// (`crate::actor::EVENT_CHANNEL_CAPACITY`).
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
    /// Panics if the background task is gone or panicked — test
    /// scaffolding, see [`fake_daemon`]'s own doc.
    pub async fn close(self) {
        let _ = self.script.send(ScriptCommand::Close).await;
        self.task.await.unwrap();
    }

    /// Arms this connection to close itself the moment it has answered its
    /// next `Request::Subscribe` and flushed anything [`Self::push`] queued
    /// beforehand — deterministically, unlike calling [`Self::close`]
    /// *before* a test's client has even issued its first real request.
    ///
    /// [`Self::close`] ends the script as soon as the background task
    /// observes it, with no regard for whether a real protocol exchange
    /// (`ListFlock`, `Subscribe`, ...) is still ahead of it — calling it
    /// before the client under test has connected races the whole
    /// arrange/act split instead of the one thing a test means to exercise.
    /// This method instead waits for the *real* milestone a "the daemon
    /// goes away mid-follow" test actually needs: the subscription this
    /// connection's caller asked for has been served, in full, before the
    /// connection ends.
    ///
    /// Synchronous is not an option here (unlike [`Self::reply_to_list`]):
    /// this arms behavior on the same background task that also drains
    /// [`Self::push`]'s queue, so it has to go through the same script
    /// channel those do.
    ///
    /// Does not consume `self`, unlike [`Self::close`]: a caller that also
    /// wants [`Self::list_flock_count`] after the connection ends needs to
    /// keep this handle alive to ask for it.
    pub async fn close_after_subscribe(&self) {
        let _ = self.script.send(ScriptCommand::CloseAfterSubscribe).await;
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
/// consumed and answers this one request; else
/// [`FakeDaemon::reply_shutting_down_then_unlink_after`] or
/// [`FakeDaemon::reply_shutting_down_and_never_unlink`], if armed, answers
/// `Response::ShuttingDown` and then unlinks `socket_path` (after a delay)
/// or leaves it in place, respectively; else a `Request::Subscribe`
/// is answered with `Response::Subscribed`, flips this connection into the
/// subscribed state, and flushes anything [`ScriptCommand::PushEvent`]
/// queued before now; else `ListFlock` is answered per
/// [`FakeDaemon::reply_to_list_sequence`]'s queue first (one entry consumed
/// per call), falling back to [`FakeDaemon::reply_to_list`] (or
/// `Flock(vec![])` if nothing is armed) once the queue is empty;
/// else `Describe { .. }` is answered per [`FakeDaemon::reply_to_describe`]
/// (or `Described(vec![])` if nothing is armed); else `Response::Pong`.
///
/// A [`ScriptCommand::PushEvent`] arriving outside a request turn is
/// written straight to the wire once subscribed, or buffered until then —
/// see [`FakeDaemon::push`].
///
/// Eleven parameters: `listener`, `socket_path` and `ack` set up the one served
/// connection, `script` carries every command a `FakeDaemon` handle can send
/// after that, and the rest are one `Arc<Mutex<..>>` slot per independently
/// armable behavior, cloned straight from [`FakeDaemon`]'s own fields —
/// bundling them into a struct would move the coupling around, not reduce
/// it, for this private, one-caller function.
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
                        // If the `Subscribe` frame was already handled by
                        // this same `select!` before this script command
                        // was dequeued (both arms are unbiased here), the
                        // flag above arms too late and the connection stays
                        // open — a latent race that surfaces as a 5s test
                        // timeout rather than a clean close. Re-check the
                        // milestone this command is arming against and act
                        // on it immediately if it already happened.
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
                        // Taken into owned locals before any `.await` below:
                        // the `MutexGuard`s `.lock()` produces are not
                        // `Send`, and holding one across an await point would
                        // make this whole future not `Send` (tokio::spawn
                        // requires `Send`).
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
                            // Run inline, in this same request arm, rather
                            // than deferred to a later `select!` turn: `kill`
                            // drops the `Client` right after reading this
                            // reply, which closes the connection — the next
                            // `frames.next()` yields `None` and the loop
                            // breaks before a deferred unlink would ever get
                            // a turn to run.
                            tokio::time::sleep(after).await;
                            let _ = std::fs::remove_file(&socket_path);
                        } else if shutdown_never_unlink.is_some() {
                            write_reply(&mut frames, envelope.id, Response::ShuttingDown).await;
                            // Deliberately never unlinks — the branch a
                            // kill-teardown timeout exists to observe.
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
                                // The sequence queue (`reply_to_list_sequence`)
                                // takes priority: a test scripting a whole
                                // sequence at once still means it, even if
                                // `reply_to_list`'s single slot happens to be
                                // armed too (it should never be armed
                                // alongside the sequence in practice, but the
                                // queue winning is the least surprising order
                                // either way).
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
/// [`Client`] alongside the still-live [`FakeDaemon`] script — for a test
/// that only needs a client past the handshake and nothing daemon-specific.
pub async fn fake_client_on(path: &Path) -> (Client, FakeDaemon) {
    fake_client_with_ack(path, sample_ack()).await
}

/// As [`fake_client_on`], but with a caller-chosen [`HelloAck`] — for a
/// test asserting on the ack a `Client` receives.
pub async fn fake_client_with_ack(path: &Path, ack: HelloAck) -> (Client, FakeDaemon) {
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
    let client = Client::connect(path).await.unwrap();
    (
        client,
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
    let mut listener = Listener::bind(path).unwrap();
    let (tx, rx) = mpsc::channel(SCRIPT_CHANNEL_CAPACITY);
    tokio::spawn(async move {
        let stream = listener.accept().await.unwrap();
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
    let mut listener = Listener::bind(path).unwrap();
    let task = tokio::spawn(async move {
        let stream = listener.accept().await.unwrap();
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
    let mut listener = Listener::bind(path).unwrap();
    let task = tokio::spawn(async move {
        let stream = listener.accept().await.unwrap();
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
    let mut listener = Listener::bind(path).unwrap();
    let task = tokio::spawn(async move {
        let stream = listener.accept().await.unwrap();
        let mut frames = Framed::new(stream, codec());
        handshake(&mut frames, sample_ack()).await;
        core::future::pending::<()>().await;
    });
    let client = Client::connect(path).await.unwrap();
    (client, task)
}

/// [`SpawnOptions`] tuned so `spawn.rs`'s tests finish in well under a
/// second on a real clock, rather than the production 30s/100ms/5s/5s
/// figures. Every test built against a real socket and (in most cases) a
/// real child process, so none of them may pause tokio's clock to fake this
/// speed — shrinking the values themselves is the only option.
///
/// The one exception, `a_child_that_dies_fails_fast_instead_of_waiting_out_the_deadline`,
/// deliberately runs on the production defaults instead: it is an assertion
/// *about* the 30s deadline, so using this would delete the thing under
/// test.
#[must_use]
pub fn fast_opts() -> SpawnOptions {
    SpawnOptions {
        deadline: Duration::from_millis(600),
        backoff_start: Duration::from_millis(10),
        backoff_cap: Duration::from_millis(50),
        handshake_timeout: Duration::from_millis(100),
    }
}

/// Binds `path`, accepts one connection, and answers its handshake with
/// [`sample_ack`], then parks — for a launcher closure that needs to bring a
/// working daemon into existence synchronously (a real launcher spawns a
/// child, this stands in for the daemon that child would eventually become).
///
/// Synchronous, not `async`: a `connect_or_spawn` launcher is a plain
/// `FnOnce() -> io::Result<Child>`, so anything it calls to make a daemon
/// "appear" has to be callable from a synchronous context too. This spawns
/// its own background task via [`tokio::spawn`], which works even though the
/// caller isn't `async`: `connect_or_spawn_with` runs the launcher on
/// `tokio::task::spawn_blocking`'s pool, and that pool's threads carry the
/// owning runtime's context for exactly this reason.
///
/// The returned task is detached deliberately: it outlives this function
/// call and keeps running for as long as the test's runtime does, which is
/// long enough for `connect_or_spawn_with`'s later probes to reach it.
///
/// Panics if `path` cannot be bound — test scaffolding, see [`fake_daemon`]'s
/// own doc for why that is the right failure mode here.
pub fn start_fake_daemon_answering_on(path: &Path) {
    let mut listener = Listener::bind(path).unwrap();
    tokio::spawn(async move {
        let stream = listener.accept().await.unwrap();
        let mut frames = Framed::new(stream, codec());
        handshake(&mut frames, sample_ack()).await;
        core::future::pending::<()>().await;
    });
}

/// A launcher-ready child that is already exiting with `code`: spawns
/// `sh -c "exit <code>"` and hands the `Child` back immediately.
///
/// For a launcher whose test cares only about `connect_or_spawn`'s reaction
/// to a child that is dead (or dying) with a known status, not about a
/// process that behaves like a daemon in any other way.
///
/// # Errors
///
/// Whatever `std::process::Command::spawn` itself can return — `sh` failing
/// to exec, principally. Propagated rather than unwrapped so a caller using
/// this directly as a launcher (`connect_or_spawn`'s `L` bound is
/// `FnOnce() -> io::Result<Child>`) gets the same signature every other
/// launcher does.
pub fn child_exiting_with(code: i32) -> std::io::Result<std::process::Child> {
    std::process::Command::new("sh")
        .args(["-c", &format!("exit {code}")])
        .spawn()
}
