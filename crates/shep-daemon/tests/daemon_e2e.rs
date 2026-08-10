//! Real-daemon integration tier: boots shep-daemon on a temp `$SHEP_HOME`,
//! talks to it over the control socket with shep-core's own codec, and
//! drives real child processes.
//!
//! Real time throughout, by necessity: these tests own real sockets and real
//! children, and a paused clock's auto-advance would expire timeouts before
//! IO wakeups arrive. IR-38 deviation deliberate — behavioral OS tests need
//! their own binary so the unit tier stays paused-clock pure.

#![cfg(unix)]

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use shep_core::config::AppConfig;
use shep_core::paths::ShepPaths;
use shep_core::protocol::{
    BusEvent, Envelope, Hello, HelloAck, HelloReply, PROTOCOL_VERSION, ProcessEventKind,
    ProcessInfo, Reply, Request, Response, RpcErrorCode, SelectorSpec, ServerFrame, codec,
    decode_frame, encode_frame,
};
use shep_core::status::ProcStatus;
use shep_core::values::UpDuration;

use shep_daemon::boot::{BootError, BootOptions, boot};
use shep_daemon::rpc::RpcContext;
use shep_daemon::tokio_runner::TokioRunner;

const RECV_TIMEOUT: Duration = Duration::from_secs(10);

/// A booted daemon on its own `$SHEP_HOME`, with its run loop spawned.
///
/// `run`/`dir` are `Option`-wrapped, not moved out directly, so this type
/// can carry a [`Drop`] impl (see below) without every existing partial
/// move (`fixture.run`, `fixture.dir`) turning into a compile error —
/// Rust forbids moving a single field out of a value whose type implements
/// `Drop`; going through `Option::take` on a `&mut self` method sidesteps
/// that without touching every call site's shape.
struct Fixture {
    dir: Option<tempfile::TempDir>,
    paths: ShepPaths,
    ctx: RpcContext,
    run: Option<tokio::task::JoinHandle<Result<(), BootError>>>,
    // Real OS pids this fixture is responsible for on the panic path — every
    // `Client::request` sharing this `Arc` records one here whenever a reply
    // carries live `ProcessInfo`s (`Started`/`Flock`/`Described`/`Restarted`/
    // `Stopped`). See `Drop` below for why this exists at all.
    spawned: std::sync::Arc<std::sync::Mutex<Vec<i32>>>,
}

impl Fixture {
    async fn boot(dir: tempfile::TempDir, restore: bool) -> Self {
        // $SHEP_HOME is the tempdir root itself: sun_path caps the socket
        // path near 104 bytes and macOS temp paths are already long.
        let home = dir.path().to_path_buf();
        let paths = ShepPaths::resolve(
            &|key| (key == "SHEP_HOME").then(|| home.display().to_string()),
            std::path::Path::new("/nonexistent"),
        );
        let daemon = boot(
            TokioRunner::new(),
            paths.clone(),
            BootOptions {
                restore,
                ..BootOptions::default()
            },
        )
        .await
        .expect("the daemon must boot on a fresh home");
        let ctx = daemon.context();
        let run = tokio::spawn(daemon.run());
        Self {
            dir: Some(dir),
            paths,
            ctx,
            run: Some(run),
            spawned: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    async fn connect(&self) -> Client {
        let stream = UnixStream::connect(&self.paths.socket).await.unwrap();
        let mut client = Client {
            frames: Framed::new(stream, codec()),
            next_id: 1,
            hello_ack: None,
            pending: std::collections::VecDeque::new(),
            spawned: self.spawned.clone(),
        };
        client
            .send(&Hello {
                client_version: env!("CARGO_PKG_VERSION").to_string(),
                protocol: PROTOCOL_VERSION,
            })
            .await;
        let ack: HelloReply = client.recv_as().await;
        client.hello_ack = Some(ack.expect("the daemon must ack our protocol"));
        client
    }

    /// Shuts the daemon down and waits for its ordered teardown.
    async fn shutdown(mut self) -> tempfile::TempDir {
        self.ctx.shutdown();
        let run = self.run.take().expect("shutdown is only ever called once");
        tokio::time::timeout(RECV_TIMEOUT, run)
            .await
            .expect("teardown must not hang")
            .unwrap()
            .unwrap();
        self.dir.take().expect("dir is only ever taken once")
    }
}

/// Last-resort net for a test that PANICS before reaching its own
/// `Fixture::shutdown()` (or, in the crash-simulation test, before
/// deliberately skipping it).
///
/// On every success path this is a no-op: `shutdown()`'s kill ladder (or,
/// in `kill_daemon_shuts_the_flock_down_and_unlinks_the_socket`, the
/// test's own explicit ESRCH poll) already reaped every tracked pid before
/// `Fixture` drops, so `kill()` below just hits ESRCH and is ignored. It
/// only does real work on the panic path — proven, not assumed, by
/// deliberately failing a test mid-run and checking for orphans (see
/// task-11-report.md's "Drop-prevents-leak experiment"): a `current_thread`
/// `#[tokio::test]` runtime that unwinds out from under a panic does NOT
/// keep polling the background `run` task to let its own async teardown
/// (the kill ladder in `RunningDaemon::run`) finish, so `ctx.shutdown()`
/// alone is not sufficient — this sends `SIGKILL` directly, synchronously,
/// with no dependency on the runtime still being alive to schedule anything.
///
/// SIGKILLs the whole process GROUP (`-pid`, not `pid`): `TokioRunner`
/// spawns every child leader in its own group (see `tokio_runner.rs`'s own
/// doc) specifically so a group signal also reaches a `sleep 1`
/// grandchild a leader-only signal would miss.
impl Drop for Fixture {
    fn drop(&mut self) {
        let pids = self
            .spawned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for &pid in pids.iter() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(-pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

/// A handshaken connection to a booted [`Fixture`], speaking shep-core's own
/// length-delimited/JSON codec directly — this tier proves the wire
/// protocol itself, not a client crate built on top of it.
struct Client {
    frames: Framed<UnixStream, LengthDelimitedCodec>,
    next_id: u64,
    hello_ack: Option<HelloAck>,
    // Frames read off the wire but not the one the CURRENT call was looking
    // for — buffered (never discarded) so a LATER call still sees them, in
    // original arrival order. Load-bearing, not defensive: the supervisor
    // actor emits a sheep's `Start`/`Online`/`Stop` bus event SYNCHRONOUSLY,
    // strictly before it resolves the RPC reply for the very command that
    // caused it (`spawn_fresh`/`handle_exited`, `supervisor.rs`) — so that
    // event routinely reaches this socket while `request` below is still
    // reading frames waiting for its own reply, race-ordered against it with
    // no scheduling guarantee either way. A `request` that silently dropped
    // non-reply frames would make a later `await_process_event` for that
    // exact event hang for the full `RECV_TIMEOUT` on genuinely correct
    // daemon behavior — reproduced empirically while writing this file: the
    // first version discarded them and hung waiting for `process.stop` after
    // a `Stop` reply that had already raced past it.
    pending: std::collections::VecDeque<ServerFrame>,
    // Shared with the owning `Fixture` — see its own doc and `Drop` impl.
    // `request` below is the one place this gets written to.
    spawned: std::sync::Arc<std::sync::Mutex<Vec<i32>>>,
}

impl Client {
    /// The daemon's handshake answer. Every [`Client`] in this file comes
    /// from [`Fixture::connect`], which always shakes hands before handing
    /// one back, so this is only ever called after that has happened.
    fn hello_ack(&self) -> &HelloAck {
        self.hello_ack
            .as_ref()
            .expect("hello_ack is only set after a successful handshake")
    }

    async fn send<T: Serialize>(&mut self, value: &T) {
        self.frames
            .send(encode_frame(value).unwrap())
            .await
            .unwrap();
    }

    /// Reads and decodes the next frame as `T`, timing out rather than
    /// hanging forever (IR-39's no-sleeps, event-driven rule).
    async fn recv_as<T: DeserializeOwned>(&mut self) -> T {
        let frame = tokio::time::timeout(RECV_TIMEOUT, self.frames.next())
            .await
            .expect("timed out waiting for a frame")
            .expect("connection closed early")
            .unwrap();
        decode_frame(&frame).unwrap()
    }

    /// The next frame of any kind: whatever an earlier call already read but
    /// didn't consume, oldest first, else the next one off the wire.
    async fn next_frame(&mut self) -> ServerFrame {
        if let Some(frame) = self.pending.pop_front() {
            return frame;
        }
        self.recv_as().await
    }

    /// Sends one request, then reads frames until its `Reply` arrives,
    /// re-queueing (never discarding — see [`Self::pending`]'s doc) any bus
    /// events that arrive in between for a later call to consume.
    async fn request(&mut self, body: Request) -> Reply {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&Envelope {
            id,
            deadline_ms: None,
            body,
        })
        .await;
        let mut skipped = Vec::new();
        let reply = loop {
            match self.next_frame().await {
                ServerFrame::Reply(reply) if reply.id == id => break reply,
                // Anything else (a bus event, an unrelated reply, or a
                // future frame kind this client doesn't know about) is set
                // aside rather than dropped — see `pending`'s own doc.
                other => skipped.push(other),
            }
        };
        requeue(&mut self.pending, skipped);
        track_spawned(&self.spawned, &reply);
        reply
    }

    /// Reads frames until a `Process` event of `kind` for `id` arrives,
    /// re-queueing (never discarding) everything else — see `pending`'s doc.
    async fn await_process_event(&mut self, id: u32, kind: ProcessEventKind) -> ProcessInfo {
        let mut skipped = Vec::new();
        let info = loop {
            let frame = self.next_frame().await;
            if let ServerFrame::Event(BusEvent::Process { event, info, .. }) = &frame
                && *event == kind
                && info.id == id
            {
                break info.clone();
            }
            skipped.push(frame);
        };
        requeue(&mut self.pending, skipped);
        info
    }

    /// Reads frames until a `LogOut` event for `id` arrives, re-queueing
    /// (never discarding) everything else — see `pending`'s doc — bounded by
    /// one overall [`RECV_TIMEOUT`], not merely `recv_as`'s own per-frame
    /// one, so a daemon that keeps emitting OTHER frames forever without
    /// ever emitting a `log.*` event for `id` cannot spin this loop past its
    /// budget. (Task 11 fix: the original version of this loop lived inline
    /// in the one test that needs it, called `next_frame` directly, and so
    /// silently discarded every non-matching frame with no deadline of its
    /// own — contradicting this exact discipline. Nothing after this call
    /// in that test reads the connection again, so discarding was harmless
    /// in practice, but the requeue treatment costs nothing and keeps every
    /// `Client` method in this file honest about the same rule.)
    async fn await_log_line(&mut self, id: u32) -> String {
        tokio::time::timeout(RECV_TIMEOUT, async {
            let mut skipped = Vec::new();
            let line = loop {
                let frame = self.next_frame().await;
                if let ServerFrame::Event(BusEvent::LogOut { id: event_id, line }) = &frame
                    && *event_id == id
                {
                    break line.clone();
                }
                skipped.push(frame);
            };
            requeue(&mut self.pending, skipped);
            line
        })
        .await
        .expect("timed out waiting for a log.* event")
    }
}

/// Records every live pid a reply's `ProcessInfo`s carry — see `Fixture`'s
/// `spawned` field and `Drop` impl for why. Every `Response` variant that
/// can carry a real spawned/listed pid is covered, not just `Started`: a
/// muster restore's fresh pids, for one, are only ever observed here via a
/// post-reboot `ListFlock` (`Response::Flock`), never a `Started` reply on
/// the rebooted client.
fn track_spawned(spawned: &std::sync::Arc<std::sync::Mutex<Vec<i32>>>, reply: &Reply) {
    let Ok(response) = &reply.result else {
        return;
    };
    let infos: &[ProcessInfo] = match response {
        Response::Flock(infos)
        | Response::Described(infos)
        | Response::Started(infos)
        | Response::Stopped(infos)
        | Response::Restarted(infos)
        | Response::Reopened(infos)
        | Response::Flushed(infos) => infos,
        _ => return,
    };
    let mut spawned = spawned
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for info in infos {
        if let Some(pid) = info.pid
            && let Ok(pid) = i32::try_from(pid)
        {
            spawned.push(pid);
        }
    }
}

/// Restores frames a call read but didn't want back onto the front of
/// `pending`, in their original arrival order, so the next call to
/// [`Client::next_frame`] sees them before reading anything new off the
/// wire.
fn requeue(pending: &mut std::collections::VecDeque<ServerFrame>, skipped: Vec<ServerFrame>) {
    for frame in skipped.into_iter().rev() {
        pending.push_front(frame);
    }
}

#[tokio::test]
async fn handshake_then_start_list_and_stop_a_real_sheep() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;
    assert_eq!(client.hello_ack().pid, std::process::id());
    assert_eq!(client.hello_ack().protocol, PROTOCOL_VERSION);

    // Subscribe BEFORE starting: the bus delivers from the moment you join.
    let subscribed = client
        .request(Request::Subscribe {
            topics: vec!["process.*".to_string()],
        })
        .await;
    assert_eq!(subscribed.result.unwrap(), Response::Subscribed);

    let mut app = AppConfig::minimal("sleeper", "/bin/sh");
    app.interpreter = Some("none".to_string());
    app.args = vec!["-c".to_string(), "while :; do sleep 1; done".to_string()];
    let started = client.request(Request::Start { apps: vec![app] }).await;
    let Response::Started(infos) = started.result.unwrap() else {
        panic!("expected started")
    };
    assert_eq!(infos.len(), 1);
    let id = infos[0].id;
    let spawned_pid = infos[0].pid.expect("a real spawn reports a real pid");

    let online = client
        .await_process_event(id, ProcessEventKind::Online)
        .await;
    assert_eq!(online.pid, Some(spawned_pid));

    let listed = client.request(Request::ListFlock).await;
    let Response::Flock(flock) = listed.result.unwrap() else {
        panic!("expected flock")
    };
    assert_eq!(flock.len(), 1);
    assert_eq!(flock[0].status, ProcStatus::Online);
    assert_eq!(flock[0].pid, Some(spawned_pid));

    let stopped = client
        .request(Request::Stop {
            selector: SelectorSpec::All,
        })
        .await;
    let Response::Stopped(gone) = stopped.result.unwrap() else {
        panic!("expected stopped")
    };
    // The reply is deferred until the kill ladder finished, so this is terminal.
    assert_eq!(gone[0].status, ProcStatus::Stopped);
    client.await_process_event(id, ProcessEventKind::Stop).await;

    fixture.shutdown().await;
}

#[tokio::test]
async fn log_lines_reach_a_log_subscriber() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;

    let subscribed = client
        .request(Request::Subscribe {
            topics: vec!["log.*".to_string()],
        })
        .await;
    assert_eq!(subscribed.result.unwrap(), Response::Subscribed);

    let mut app = AppConfig::minimal("chatty", "/bin/sh");
    app.interpreter = Some("none".to_string());
    app.args = vec!["-c".to_string(), "echo hello-flock; sleep 5".to_string()];
    let started = client.request(Request::Start { apps: vec![app] }).await;
    let Response::Started(infos) = started.result.unwrap() else {
        panic!("expected started")
    };
    let id = infos[0].id;

    let line = client.await_log_line(id).await;
    assert_eq!(line, "hello-flock");

    fixture.shutdown().await;
}

/// Waits for `path` to hold exactly `expected`, failing at [`RECV_TIMEOUT`].
///
/// Polls rather than sleeping a fixed guess. A line observed on the bus has
/// had its file write ISSUED, not necessarily completed — `tokio::fs`
/// dispatches the real `write(2)` to the blocking pool — so this waits for
/// the write to land instead of assuming it already has. Duplicated from
/// `real_runner.rs` rather than shared: integration binaries are separate
/// crates, as that file's own helpers already note.
async fn await_file_contents(path: &std::path::Path, expected: &str) {
    let settled = tokio::time::timeout(RECV_TIMEOUT, async {
        while std::fs::read_to_string(path).unwrap_or_default() != expected {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        settled.is_ok(),
        "{}: expected {expected:?}, found {:?}",
        path.display(),
        std::fs::read_to_string(path)
    );
}

/// `create`-mode rotation, end to end: rename the live log, ask the daemon
/// over its own socket, and watch the sheep's next line land on the
/// recreated path.
///
/// Fails if the request never reaches the sheep's log pump — which is the
/// whole of this verb, and which the engine tier cannot show: the scripted
/// fake writes no files, so there every wiring that answers `Ok` looks
/// alike. Here a `Reopen` that resolved the selector and pushed nothing
/// leaves the pump on the renamed inode, the live path missing, and the
/// second line invisible to anything reading the log.
///
/// Both halves are asserted for the reason `real_runner.rs`'s own reopen
/// case gives: a pump that opened a second handle without dropping the
/// first would grow the new file too, and only the archive standing still
/// rules that out.
///
/// The sheep's own log path is read off the `Started` reply rather than
/// derived here, so the test cannot disagree with the daemon about which
/// file it is looking at.
#[tokio::test]
async fn reopen_moves_a_running_sheeps_log_onto_the_recreated_path() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;

    // Subscribe BEFORE starting: a connection gets no forwarder task, and so
    // no events at all, until it does.
    let subscribed = client
        .request(Request::Subscribe {
            topics: vec!["log.*".to_string()],
        })
        .await;
    assert_eq!(subscribed.result.unwrap(), Response::Subscribed);

    // The marker lets the test decide when the second line happens, so
    // "after the reopen" is a fact rather than a timing bet. `sleep`'s only
    // portable argument is a whole number of seconds (POSIX), which is why
    // the poll is that coarse.
    let marker = fixture.paths.home.join("go");
    let mut app = AppConfig::minimal("rotator", "/bin/sh");
    app.interpreter = Some("none".to_string());
    app.args = vec![
        "-c".to_string(),
        format!(
            "echo before; while [ ! -f {} ]; do sleep 1; done; echo after; sleep 5",
            marker.display()
        ),
    ];
    let started = client.request(Request::Start { apps: vec![app] }).await;
    let Response::Started(infos) = started.result.unwrap() else {
        panic!("expected started")
    };
    let id = infos[0].id;
    let out_file = std::path::PathBuf::from(
        infos[0]
            .out_file
            .clone()
            .expect("this daemon reports its own resolved log paths"),
    );

    assert_eq!(client.await_log_line(id).await, "before");
    await_file_contents(&out_file, "before\n").await;

    let archive = out_file.with_extension("log.1");
    std::fs::rename(&out_file, &archive).unwrap();
    assert!(!out_file.exists(), "sanity: the rename really moved it");

    let reopened = client
        .request(Request::Reopen {
            selector: SelectorSpec::All,
        })
        .await;
    let Response::Reopened(matched) = reopened.result.unwrap() else {
        panic!("expected reopened")
    };
    assert_eq!(matched.len(), 1);
    assert_eq!(matched[0].id, id);

    // The reply is the barrier: it lands only after the pump has flushed
    // the old handle and opened the path again, so neither of these polls.
    assert_eq!(std::fs::read_to_string(&out_file).unwrap(), "");
    assert_eq!(std::fs::read_to_string(&archive).unwrap(), "before\n");

    std::fs::write(&marker, "").unwrap();
    assert_eq!(client.await_log_line(id).await, "after");
    await_file_contents(&out_file, "after\n").await;
    assert_eq!(
        std::fs::read_to_string(&archive).unwrap(),
        "before\n",
        "the renamed file must stop growing the moment the handle is swapped"
    );

    fixture.shutdown().await;
}

/// What the flush case writes at the live log path after renaming the real
/// one away — standing in for the file a `create`-mode rotator leaves behind,
/// and the thing that must be gone afterwards.
///
/// One owner: the case asserts both that this is gone from one file and that
/// it never reached the other, and a second copy could drift between them.
const STRAY_CONTENT: &str = "what the recreated log holds\n";

/// `flush` resolves to the RECORDED PATH, never to the inode the pump is
/// holding — the one thing about this verb that only a real pump on a real
/// file can show.
///
/// The rename is what separates the two. Afterwards the sheep's log pump
/// still has the archive open (nothing reopened it), while the path the
/// daemon recorded at registration now names a different file. An
/// implementation that emptied the pump's own handle — by `set_len(0)` on it,
/// or by asking the pump to truncate what it holds — would empty the ARCHIVE
/// and leave the live log untouched: the exact opposite of what was asked,
/// exiting 0 while doing it. That is the shape of failure this case exists
/// for, and both assertions are needed to catch it, since either one alone
/// still passes under the inversion.
///
/// The stray content is written at the live path deliberately. Without it the
/// live path would simply be missing after the rename, the truncate would be
/// the documented no-op, and an inode-chasing implementation would look
/// identical from the outside.
///
/// The paths are read off the `Started` reply rather than derived here, so
/// the test cannot disagree with the daemon about which file it is renaming.
#[tokio::test]
async fn flush_empties_the_recorded_path_and_leaves_a_renamed_archive_alone() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;

    // Subscribe BEFORE starting: a connection gets no forwarder task, and so
    // no events at all, until it does.
    let subscribed = client
        .request(Request::Subscribe {
            topics: vec!["log.*".to_string()],
        })
        .await;
    assert_eq!(subscribed.result.unwrap(), Response::Subscribed);

    let mut app = AppConfig::minimal("noisy", "/bin/sh");
    app.interpreter = Some("none".to_string());
    app.args = vec!["-c".to_string(), "echo before; sleep 5".to_string()];
    let started = client.request(Request::Start { apps: vec![app] }).await;
    let Response::Started(infos) = started.result.unwrap() else {
        panic!("expected started")
    };
    let id = infos[0].id;
    let out_file = std::path::PathBuf::from(
        infos[0]
            .out_file
            .clone()
            .expect("this daemon reports its own resolved log paths"),
    );

    assert_eq!(client.await_log_line(id).await, "before");
    await_file_contents(&out_file, "before\n").await;

    // From here the pump's handle and the recorded path name different
    // files, which is the whole point of the case.
    let archive = out_file.with_extension("log.1");
    std::fs::rename(&out_file, &archive).unwrap();
    std::fs::write(&out_file, STRAY_CONTENT).unwrap();

    let flushed = client
        .request(Request::Flush {
            selector: SelectorSpec::All,
        })
        .await;
    let Response::Flushed(matched) = flushed.result.unwrap() else {
        panic!("expected flushed")
    };
    assert_eq!(matched.len(), 1);
    assert_eq!(matched[0].id, id);

    // The reply is the barrier: it lands only once every matched pump has
    // answered and every recorded path has been truncated, so neither of
    // these polls.
    assert_eq!(
        std::fs::read_to_string(&out_file).unwrap(),
        "",
        "the recorded path is what a flush empties"
    );
    assert_eq!(
        std::fs::read_to_string(&archive).unwrap(),
        "before\n",
        "the renamed file is not the daemon's to empty — a flush that chased \
         the pump's inode would have emptied this one instead"
    );

    fixture.shutdown().await;
}

/// How long this test waits for the gated sheep's `Online`. Generous for a
/// loaded runner, but a small fraction of the `listen_timeout` below — the
/// gap between the two is the whole assertion.
const READY_DEADLINE: Duration = Duration::from_secs(5);

/// A `wait_ready` sheep must go online off its OWN `{"kind":"ready"}` write,
/// not off the deadline that follows it.
///
/// This is the only test in the workspace that drives a real child's fd 3
/// all the way through `run_sheep`'s `ChildMessage::Ready -> Msg::Ready`
/// forward to the readiness wait. The unit tier pushes `Msg::Ready` into the
/// actor's mailbox directly — downstream of that forward — so deleting the
/// forward leaves the whole unit tier green while every `wait_ready` app in
/// production sits at `starting` for its entire `listen_timeout`.
///
/// `listen_timeout` is set two orders of magnitude past [`READY_DEADLINE`]
/// on purpose. A gated sheep reaches `online` eventually either way (an
/// elapsed readiness deadline brings the sheep online rather than failing it —
/// see the supervisor's `handle_ready_result`), so only an `Online` that
/// arrives EARLY can tell a forwarded ready message apart from an expired one.
/// Nothing else can, in this tier: the deadline's own `warn!` is rendered only
/// by the subscriber `shep-cli`'s `daemon` subcommand installs, and this file
/// boots the library directly, so the two paths produce the same event, the
/// same status, and no output either way.
#[tokio::test]
async fn a_wait_ready_sheep_goes_online_on_its_own_channel_message() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;

    // Subscribe BEFORE starting: the bus delivers from the moment you join.
    let subscribed = client
        .request(Request::Subscribe {
            topics: vec!["process.*".to_string()],
        })
        .await;
    assert_eq!(subscribed.result.unwrap(), Response::Subscribed);

    let mut app = AppConfig::minimal("greeter", "/bin/sh");
    app.interpreter = Some("none".to_string());
    // `wait_ready` is what makes `assemble` open fd 3 at all, so the same
    // flag arms the gate and gives the child something to write to.
    app.wait_ready = true;
    app.args = vec![
        "-c".to_string(),
        r#"printf '{"kind":"ready"}\n' >&3; while :; do sleep 1; done"#.to_string(),
    ];
    app.listen_timeout = UpDuration::from_millis(600_000);

    let started = client.request(Request::Start { apps: vec![app] }).await;
    let Response::Started(infos) = started.result.unwrap() else {
        panic!("expected started")
    };
    let id = infos[0].id;
    assert_eq!(
        infos[0].status,
        ProcStatus::Starting,
        "a gated sheep is `starting` when Start replies, never `online`"
    );

    let online = tokio::time::timeout(
        READY_DEADLINE,
        client.await_process_event(id, ProcessEventKind::Online),
    )
    .await
    .expect("the child's own ready message must bring the sheep online");
    assert_eq!(online.status, ProcStatus::Online);

    let listed = client.request(Request::ListFlock).await;
    let Response::Flock(flock) = listed.result.unwrap() else {
        panic!("expected flock")
    };
    assert_eq!(flock[0].status, ProcStatus::Online);

    fixture.shutdown().await;
}

#[tokio::test]
async fn protocol_skew_is_refused_over_the_real_socket() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;

    // Built by hand, not through `Fixture::connect`: that helper always
    // sends a MATCHING protocol, and this test needs to send a mismatched
    // one instead.
    let stream = UnixStream::connect(&fixture.paths.socket).await.unwrap();
    let mut frames = Framed::new(stream, codec());
    frames
        .send(
            encode_frame(&Hello {
                client_version: "9.9.9".to_string(),
                protocol: PROTOCOL_VERSION + 1,
            })
            .unwrap(),
        )
        .await
        .unwrap();

    let frame = tokio::time::timeout(RECV_TIMEOUT, frames.next())
        .await
        .expect("timed out waiting for the refusal")
        .expect("connection closed before refusing")
        .unwrap();
    let ack: HelloReply = decode_frame(&frame).unwrap();
    let err = ack.expect_err("protocol skew must be refused, not silently accepted");
    assert_eq!(err.code, RpcErrorCode::ProtocolMismatch);

    let eof = tokio::time::timeout(RECV_TIMEOUT, frames.next())
        .await
        .expect("timed out waiting for the connection to close");
    assert!(
        eof.is_none(),
        "the daemon must close the connection after refusing skew"
    );

    fixture.shutdown().await;
}

#[tokio::test]
async fn kill_daemon_shuts_the_flock_down_and_unlinks_the_socket() {
    let mut fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;

    let sleeper = |name: &str| {
        let mut app = AppConfig::minimal(name, "/bin/sh");
        app.interpreter = Some("none".to_string());
        app.args = vec!["-c".to_string(), "while :; do sleep 1; done".to_string()];
        app
    };
    let started = client
        .request(Request::Start {
            apps: vec![sleeper("one"), sleeper("two")],
        })
        .await;
    let Response::Started(infos) = started.result.unwrap() else {
        panic!("expected started")
    };
    let pids: Vec<i32> = infos
        .iter()
        .map(|i| i32::try_from(i.pid.expect("a real spawn reports a real pid")).unwrap())
        .collect();
    assert_eq!(pids.len(), 2);

    let killed = client.request(Request::KillDaemon).await;
    assert_eq!(killed.result.unwrap(), Response::ShuttingDown);

    let socket = fixture.paths.socket.clone();
    let pidfile_path = shep_daemon::boot::pidfile(&fixture.paths);
    let run = fixture.run.take().expect("run is only ever taken once");
    tokio::time::timeout(RECV_TIMEOUT, run)
        .await
        .expect("teardown must not hang")
        .unwrap()
        .unwrap();

    assert!(
        !socket.exists(),
        "the control socket must be unlinked on teardown"
    );
    assert!(
        !pidfile_path.exists(),
        "the pidfile must be removed on teardown"
    );

    // What this proves: the daemon's kill ladder REAPED both DIRECT
    // children -- the two `/bin/sh` leaders `pids` holds -- not merely
    // signaled them. `kill(pid, None)` still returns `Ok` for a zombie
    // (exited but not yet `wait()`ed by its parent — the pid stays in the
    // process table until reaped), so only a transition all the way to
    // ESRCH proves the daemon's own `wait()` actually ran, which is what
    // `assert_reaped` polls for instead of sleeping a fixed guess.
    //
    // What this does NOT prove: that their `sleep 1` GRANDCHILDREN are
    // also gone. Neither `pids` nor anything else in this test ever learns
    // those pids (spec §7's shepherd channel never reports grandchild
    // pids either), so this loop cannot poll them directly — the kill
    // ladder reaches them via the process-GROUP signals both its rungs send
    // (`tokio_runner.rs`'s `signal_group`), which this test does not
    // independently verify. `real_runner.rs`'s
    // `a_graceful_stop_reaches_a_forked_grandchild` is where that is proven,
    // against a pid it learns from the wrapper itself.
    for pid in pids {
        assert_reaped(pid).await;
    }

    // Engine unreachable: a fresh connect on the now-unlinked socket path
    // must fail, not hang or succeed against a daemon that never really left.
    assert!(
        UnixStream::connect(&socket).await.is_err(),
        "the daemon must not still be answering after KillDaemon"
    );
}

/// Polls `kill(pid, None)` for ESRCH (no such process) instead of sleeping a
/// fixed guess — see the comment at this fn's one multi-line call site
/// (`kill_daemon_shuts_the_flock_down_and_unlinks_the_socket`) for exactly
/// what a transition to ESRCH does and does not prove.
async fn assert_reaped(pid: i32) {
    let reaped = tokio::time::timeout(RECV_TIMEOUT, async {
        loop {
            match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
                Err(nix::errno::Errno::ESRCH) => break,
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await;
    assert!(
        reaped.is_ok(),
        "pid {pid} must be reaped by teardown's kill ladder"
    );
}

#[tokio::test]
async fn a_socket_left_behind_by_a_crash_does_not_block_the_next_boot() {
    let mut fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let socket = fixture.paths.socket.clone();

    // Simulate a crash: abort the run loop instead of asking for its
    // ordered teardown, so neither the socket file nor the pidfile is ever
    // unlinked. Awaiting the now-aborted handle (rather than a fixed sleep)
    // is what makes this deterministic: it only resolves once the task —
    // and the `UnixListener` it owned — has actually finished dropping, so
    // the reboot below's stale-socket probe can never race a listener that
    // hasn't finished closing yet.
    let run = fixture.run.take().expect("run is only ever taken once");
    run.abort();
    let outcome = run.await;
    assert!(
        outcome.is_err_and(|err| err.is_cancelled()),
        "the run task must have been cancelled, not completed on its own"
    );
    assert!(
        socket.exists(),
        "sanity: a crash leaves the socket file behind"
    );

    // Same `$SHEP_HOME`: `dir` is taken out of `fixture` here rather than
    // dropped alongside it, so the directory (and the leftover socket file
    // inside it) survives into the reboot. No processes were ever started
    // on this fixture, so its `Drop` (see `Fixture`'s own doc) has nothing
    // to reap when it goes out of scope at the end of this fn.
    let dir = fixture.dir.take().expect("dir is only ever taken once");
    let rebooted = Fixture::boot(dir, false).await;
    let mut client = rebooted.connect().await;
    let pong = client.request(Request::Ping).await;
    assert_eq!(pong.result.unwrap(), Response::Pong);

    rebooted.shutdown().await;
}

#[tokio::test]
async fn muster_restores_the_flock_across_a_daemon_lifetime() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;

    let mut alpha = AppConfig::minimal("alpha", "/bin/sh");
    alpha.interpreter = Some("none".to_string());
    alpha.args = vec!["-c".to_string(), "while :; do sleep 1; done".to_string()];
    let mut beta = AppConfig::minimal("beta", "/bin/sh");
    beta.interpreter = Some("none".to_string());
    beta.instances = 2;
    beta.args = vec!["-c".to_string(), "while :; do sleep 1; done".to_string()];
    let started = client
        .request(Request::Start {
            apps: vec![alpha, beta],
        })
        .await;
    let Response::Started(before) = started.result.unwrap() else {
        panic!("expected started")
    };
    assert_eq!(before.len(), 3, "alpha (1 instance) + beta (2 instances)");
    let old_pids: std::collections::HashSet<u32> = before.iter().map(|i| i.pid.unwrap()).collect();

    // Explicit write, no polling: the roll write is a call, not a race.
    fixture.ctx.snapshot_now().await.unwrap();
    let roll = shep_daemon::snapshot::read(&fixture.paths.snapshot).unwrap();
    let running_by_name: std::collections::HashMap<_, _> = roll
        .apps
        .iter()
        .map(|a| (a.app.name.clone(), a.instances_running))
        .collect();
    assert_eq!(running_by_name.get("alpha"), Some(&1));
    assert_eq!(running_by_name.get("beta"), Some(&2));

    let dir = fixture.shutdown().await; // same $SHEP_HOME survives the reboot

    // shutdown()'s kill ladder must have actually reaped the pre-reboot
    // flock, not merely recorded it in the roll as it was — test 4
    // (kill_daemon_shuts_the_flock_down_and_unlinks_the_socket) already
    // proves teardown's kill ladder in general; this confirms it held for
    // THIS fixture's own three sheep too, before trusting the "a restored
    // sheep gets a fresh pid" assertion below to mean anything (a stale
    // pid the OS happened not to reuse yet would make that assertion pass
    // for the wrong reason).
    for &pid in &old_pids {
        assert_reaped(i32::try_from(pid).unwrap()).await;
    }

    let rebooted = Fixture::boot(dir, true).await;
    let listed = rebooted.connect().await.request(Request::ListFlock).await;
    let Response::Flock(after) = listed.result.unwrap() else {
        panic!("expected flock")
    };
    assert_eq!(
        after.len(),
        3,
        "both apps' full instance counts must come back"
    );
    for info in &after {
        assert_eq!(info.status, ProcStatus::Online);
        let pid = info.pid.expect("a restored sheep is a real live process");
        assert!(
            !old_pids.contains(&pid),
            "a restored sheep gets a fresh pid, id {}",
            info.id
        );
    }
    rebooted.shutdown().await;
}

/// RAII guard: prepends `dir` to the current `PATH` for one test's
/// duration, restoring the exact original value on drop (including on
/// panic).
///
/// Prepending, never REPLACING, is what keeps this safe under this file's
/// own parallel test harness: unlike `real_runner.rs`'s `PathGuard` (whose
/// other tests hand-build a `SpawnSpec` with an empty `env` and so never
/// read `PATH` at all), every OTHER test in `daemon_e2e.rs` boots a real
/// daemon that calls `assemble()`'s `base_env()`, which DOES read `PATH` —
/// a concurrently-running sleeper's `/bin/sh -c "while :; do sleep 1; done"`
/// needs `sleep` (an external binary, resolved through the CHILD shell's own
/// inherited `PATH`) to still be findable while this guard is active.
/// Prepending keeps every real entry reachable throughout; only replacing
/// the whole value would starve another test's concurrent spawn.
///
/// # Why `unsafe` here doesn't touch the crate's own `#![deny(unsafe_code)]`
///
/// `tests/daemon_e2e.rs` compiles as its own crate root, not part of the
/// `shep-daemon` library `lib.rs` gates. `std::env::set_var`/`remove_var`'s
/// documented hazard is an OS thread doing a raw, std-UNSYNCHRONIZED
/// `getenv` at the same instant; every read of `PATH` anywhere in this
/// binary goes through `std::env::var`, which std itself serializes against
/// `set_var`/`remove_var` internally — nothing here (or in any dependency
/// this test exercises) calls a raw, unsynchronized libc `getenv`.
struct PathGuard {
    original: Option<String>,
}

impl PathGuard {
    fn prepend(dir: &std::path::Path) -> Self {
        let original = std::env::var("PATH").ok();
        let combined = match &original {
            Some(existing) => format!("{}:{existing}", dir.display()),
            None => dir.display().to_string(),
        };
        // SAFETY: see struct doc.
        unsafe { std::env::set_var("PATH", combined) };
        Self { original }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        match &self.original {
            // SAFETY: see struct doc.
            Some(value) => unsafe { std::env::set_var("PATH", value) },
            // SAFETY: see struct doc.
            None => unsafe { std::env::remove_var("PATH") },
        }
    }
}

#[tokio::test]
async fn a_bare_interpreter_resolves_via_the_inherited_path() {
    // Deviation from the brief's literal bare-`"sh"` version (adversarial
    // finding #1 regression test): empirically confirmed (`rustc`-compiled
    // probe, `Command::new("sh")` + `env_clear()`, no PATH key at all —
    // still exits 0) that a bare `"sh"` resolves via glibc/libSystem's
    // `execvp` OS-level fallback (`_PATH_DEFPATH`, `/usr/bin:/bin` on
    // macOS/BSD) whenever PATH is ABSENT from the child's env — exactly the
    // env `tokio_runner.rs` hands the child if `assemble()`'s `base_env()`
    // fix regresses. That means a literal bare `"sh"` test would stay GREEN
    // even with the fix reverted: the same pitfall `real_runner.rs`'s own
    // `a_bare_interpreter_resolves_via_the_seeded_path` already discovered
    // and fixed for the assemble()+runner tier (see that test's own doc). A
    // throwaway-tempdir shim, never on that OS default path, is the only
    // interpreter name that can make this test fail if the fix regresses —
    // this is the same technique, now proven through the FULL daemon RPC
    // stack (Start-over-socket -> supervisor -> assemble() -> TokioRunner)
    // rather than calling assemble()+TokioRunner directly.
    use std::os::unix::fs::PermissionsExt as _;

    let shim_home = tempfile::tempdir().unwrap();
    let shim_dir = shim_home.path().join("bin");
    std::fs::create_dir_all(&shim_dir).unwrap();
    let shim_path = shim_dir.join("shep-test-interp");
    std::fs::write(&shim_path, "#!/bin/sh\necho shep-bare-interpreter-ok\n").unwrap();
    let mut perms = std::fs::metadata(&shim_path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&shim_path, perms).unwrap();

    // This test is the only one in this binary that mutates PATH; see
    // PathGuard's own doc for why prepending (not replacing) is safe
    // alongside every other, concurrently-running test here.
    let _path_guard = PathGuard::prepend(&shim_dir);

    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;
    // Subscribe BEFORE starting (as test 1 does): a connection gets no
    // forwarder task, and so no events at all, until it does. The brief's
    // own version of this test omitted this and would hang the same way —
    // caught empirically, not assumed, while writing this file.
    let subscribed = client
        .request(Request::Subscribe {
            topics: vec!["process.*".to_string()],
        })
        .await;
    assert_eq!(subscribed.result.unwrap(), Response::Subscribed);

    // Bare: only found via the seeded PATH now that it includes shim_dir.
    let mut app = AppConfig::minimal("bare", "unused");
    app.interpreter = Some("shep-test-interp".to_string());
    let started = client.request(Request::Start { apps: vec![app] }).await;
    let Response::Started(infos) = started.result.unwrap() else {
        panic!("expected started")
    };
    let id = infos[0].id;

    // A failed exec (ENOENT from a PATH that never reaches the shim) lands
    // the sheep in Errored, never Online — reaching Online is the
    // load-bearing assertion.
    let online = client
        .await_process_event(id, ProcessEventKind::Online)
        .await;
    assert_eq!(online.status, ProcStatus::Online);

    fixture.shutdown().await;
}
