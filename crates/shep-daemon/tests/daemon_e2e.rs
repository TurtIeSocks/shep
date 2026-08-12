//! Real-daemon integration tier: boots shep-daemon on a temp `$SHEP_HOME`,
//! talks to it over the control socket with shep-core's own codec, and
//! drives real child processes.
//!
//! Real time throughout, by necessity: these tests own real sockets and real
//! children, and a paused clock's auto-advance would expire timeouts before
//! IO wakeups arrive. IR-38 deviation deliberate — behavioral OS tests need
//! their own binary so the unit tier stays paused-clock pure.

#![cfg(unix)]

use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt as _;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use shep_core::config::AppConfig;
use shep_core::paths::ShepPaths;
use shep_core::protocol::{
    ActionOutcome, BusEvent, Envelope, Hello, HelloAck, HelloReply, PROTOCOL_VERSION,
    ProcessEventKind, ProcessInfo, Reply, Request, Response, RpcErrorCode, SelectorSpec,
    ServerFrame, codec, decode_frame, encode_frame,
};
use shep_core::status::ProcStatus;
use shep_core::values::UpDuration;

use shep_daemon::boot::{BootError, BootOptions, DIR_MODE, boot};
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
    ///
    /// Every process event that passes through records its pid for `Fixture`'s
    /// panic-path cleanup, the same way [`track_spawned`] records a reply's.
    /// One choke point covers every wait in this file, which matters for the
    /// one process no reply ever names in time: a reload's replacement is
    /// spawned, and can be left an orphan by a panic, well before any reply a
    /// test asks for carries its pid. Observed, not theorised — a deliberately
    /// broken kill ladder timed a reload measurement out and left one
    /// `reuse_port_sheep` reparented to init.
    async fn next_frame(&mut self) -> ServerFrame {
        let frame = match self.pending.pop_front() {
            Some(frame) => frame,
            None => self.recv_as().await,
        };
        if let ServerFrame::Event(BusEvent::Process { info, .. }) = &frame {
            track_pid(&self.spawned, info);
        }
        frame
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

    /// Reads frames until a `Process` event of `kind` arrives for ANY sheep,
    /// re-queueing (never discarding) everything else — see `pending`'s doc.
    ///
    /// The id-blind twin of [`Self::await_process_event`], for the one event
    /// whose subject a caller cannot name in advance: a reload's replacement
    /// is allocated a fresh id that first reaches the client on the event
    /// itself.
    async fn await_any_process_event(&mut self, kind: ProcessEventKind) -> ProcessInfo {
        let mut skipped = Vec::new();
        let info = loop {
            let frame = self.next_frame().await;
            if let ServerFrame::Event(BusEvent::Process { event, info, .. }) = &frame
                && *event == kind
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
        | Response::Reloading(infos)
        | Response::Reopened(infos)
        | Response::Flushed(infos) => infos,
        _ => return,
    };
    for info in infos {
        track_pid(spawned, info);
    }
}

/// Records one `ProcessInfo`'s pid, if it has one. The single writer behind
/// both [`track_spawned`] and every process event [`Client::next_frame`] sees.
fn track_pid(spawned: &std::sync::Arc<std::sync::Mutex<Vec<i32>>>, info: &ProcessInfo) {
    if let Some(pid) = info.pid
        && let Ok(pid) = i32::try_from(pid)
    {
        spawned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(pid);
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

/// A reopen asked for over the socket puts a REMOVED log directory back at
/// [`DIR_MODE`], the mode every directory shep creates is worth — the case a
/// rotator that moves the directory aside rather than the files produces.
///
/// Fails if the pump's own directory creation stops asking `mkdir` for the
/// mode: swapping `open_append`'s `DirBuilder::new().mode(DIR_MODE)` back to
/// a plain `create_dir_all` recreates the directory at `0o777` narrowed by
/// whatever the ambient umask strips — `0o755` under the common `umask 022` —
/// and the mode assertion below reddens on the difference. Dropping the
/// creation altogether reddens the assertions around it instead: the reopen
/// answers `ReopenFailed` for a path whose parent is gone, and the sheep's
/// next line has nowhere to land.
///
/// One umask cannot be distinguished. Under `umask 0o077` a plain
/// `create_dir_all` lands `0o700` unaided and both implementations look
/// alike here. That is a property of the ambient umask rather than of the
/// code, and the only way to remove it is for the test to set a process-wide
/// umask — `unsafe`, and it would leak into every other case in this binary.
///
/// The mode assertion needs no `#[cfg]` of its own: this file is
/// `#![cfg(unix)]` at its root, so `DIR_MODE` and `PermissionsExt` are only
/// ever compiled where they mean something and `--all-targets` never builds
/// this binary on the Windows leg.
///
/// No `ScriptedRunner`, so there are no scripts to size — this tier runs the
/// real runner, and the fixture is ONE sheep needing ONE real spawn. The
/// scripted fake is not merely awkward here but blind: it writes no files, so
/// its pump answers a reopen `Ok` whether or not any directory exists.
#[tokio::test]
async fn reopen_recreates_a_removed_log_directory_owner_only() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;

    // The marker lives beside the log directory, not inside it, so removing
    // that directory below cannot disturb it. `sleep`'s only portable
    // argument is a whole number of seconds (POSIX), which is why the sheep's
    // own poll is that coarse.
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
    await_file_contents(&out_file, "before\n").await;

    // The whole directory, not the file. That is what a rotator moving
    // `logs/` aside leaves behind, and it is the only shape in which the mode
    // of a freshly created directory is observable at all — `mkdir`'s mode
    // governs the directories a call creates, never one already there.
    std::fs::remove_dir_all(&fixture.paths.logs).unwrap();
    assert!(
        !fixture.paths.logs.exists(),
        "sanity: the log directory really is gone"
    );

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

    let mode = std::fs::metadata(&fixture.paths.logs)
        .expect("a reopen must put the log directory back")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, DIR_MODE,
        "the recreated log directory must be {DIR_MODE:o}, found {mode:o}"
    );

    // The reply is the barrier — both handles are open on the recreated path
    // by the time it lands — so the sheep's next line is what says the
    // directory is usable and not merely present.
    std::fs::write(&marker, "").unwrap();
    await_file_contents(&out_file, "after\n").await;

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

/// A real child answers a triggered action over its own fd 3, twice in a
/// row.
///
/// The paused-clock tier cannot reach this at all: `ScriptedRunner`
/// (`fake.rs`) is driven entirely off in-process `tokio::sync::mpsc`
/// channels, so nothing there ever crosses a real socketpair, and nothing
/// there can catch a regression in the fd-3 wiring itself — the
/// `SHEP_CHANNEL_FD`/`fd_mappings` half in `tokio_runner.rs`, or the
/// newline-JSON framing `spawn_channel_pumps` puts on the wire. This is the
/// one place in the workspace that can.
///
/// Two round trips, not one, because a single exchange is not evidence: it
/// was measured, while fixing the fd-3 blocking bug this test now guards,
/// that a one-round-trip version of this test ran 25/25 green against the
/// UNFIXED daemon — a single reply can land by winning a spawn-timing race
/// even on a build that cannot really do this at all. A second exchange on
/// the SAME live channel is what a race cannot fake twice.
///
/// The child echoes a counter rather than a fixed string, so the two
/// replies are also distinguishable from each other — a build that answered
/// every trigger with whatever the child happened to have buffered first
/// would still pass a same-body assertion.
///
/// # What this does NOT try to prove
///
/// That a successful `to_child.send()` is delivery. It measurably is not —
/// see `begin_action`'s own doc in `supervisor.rs`: the first send after a
/// child has died is accepted and discarded, and only the second one
/// errors. So nothing here ever asserts on a send's own `Ok`/`Err`; the only
/// proof either round trip landed is the `Replied` row itself, read back off
/// the RPC reply.
#[tokio::test]
async fn a_triggered_action_reaches_a_real_child_and_answers_it_twice() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;

    let mut app = AppConfig::minimal("responder", "/bin/sh");
    app.interpreter = Some("none".to_string());
    // `channel` is what `assemble()` needs to open fd 3 at all here — unlike
    // the `wait_ready` fixture above, this app never gates readiness on it.
    app.channel = true;
    app.args = vec![
        "-c".to_string(),
        r#"i=0; while IFS= read -r _line <&3; do i=$((i + 1)); printf '{"kind":"action-reply","action":"gc","body":"round-%d"}\n' "$i" >&3; done"#
            .to_string(),
    ];
    let started = client.request(Request::Start { apps: vec![app] }).await;
    let Response::Started(infos) = started.result.unwrap() else {
        panic!("expected started")
    };
    let id = infos[0].id;

    for round in 1..=2 {
        let triggered = client
            .request(Request::Trigger {
                selector: SelectorSpec::Id(id),
                action: "gc".to_string(),
                params: None,
            })
            .await;
        let Response::Triggered(rows) = triggered.result.unwrap() else {
            panic!("expected triggered")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(
            rows[0].outcome,
            ActionOutcome::Replied {
                body: format!("round-{round}"),
            },
            "round {round} must carry its own reply, not a leftover from the other one"
        );
    }

    fixture.shutdown().await;
}

/// A trigger against a sheep with no shepherd channel is refused per-row,
/// naming the missing channel, rather than waiting out a timeout for a
/// reply that was never coming.
///
/// `AppConfig::minimal` leaves `channel`, `wait_ready`, and
/// `shutdown_with_message` all false — the only three things `assemble()`
/// ORs together to decide whether a sheep gets fd 3 at all
/// (`assemble.rs`'s own doc) — so this app is spawned with no channel from
/// the start.
///
/// Only the real runner can show what this test is actually about: with
/// `spec.channel == false`, `tokio_runner.rs` never spawns a writer task for
/// this sheep and drops the send side's receiving end at spawn
/// (`drop(from_child_tx); drop(to_child_rx);`), so `spawn_action_task`'s own
/// `to_child.send()` fails AT ONCE and the row comes back `NoChannel`
/// without ever arming a wait. The paused-clock tier can only assert the
/// same OUTCOME by building a `SheepSlot` with `to_child: None` by hand
/// (`supervisor.rs`'s `a_sheep_with_no_channel_is_refused_in_its_own_row`
/// and its siblings) — which proves the row-aggregation logic but not this
/// send-fails-fast wiring underneath it.
#[tokio::test]
async fn a_trigger_against_a_channelless_sheep_names_the_missing_channel() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;

    let mut app = AppConfig::minimal("mute", "/bin/sh");
    app.interpreter = Some("none".to_string());
    app.args = vec!["-c".to_string(), "while :; do sleep 1; done".to_string()];
    let started = client.request(Request::Start { apps: vec![app] }).await;
    let Response::Started(infos) = started.result.unwrap() else {
        panic!("expected started")
    };
    let id = infos[0].id;

    let triggered = client
        .request(Request::Trigger {
            selector: SelectorSpec::Id(id),
            action: "gc".to_string(),
            params: None,
        })
        .await;
    let Response::Triggered(rows) = triggered.result.unwrap() else {
        panic!("expected triggered")
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, id);
    assert_eq!(
        rows[0].outcome,
        ActionOutcome::NoChannel,
        "a sheep spawned with no channel must be refused by name, not waited out"
    );

    fixture.shutdown().await;
}

/// A trigger against a child that reads its action and then says nothing
/// gets the daemon's own `TimedOut` row back — never a client-side
/// `DeadlineExceeded`, which would name no sheep at all.
///
/// `action_timeout` is set well under both this file's own [`RECV_TIMEOUT`]
/// and `rpc.rs`'s `DEFAULT_DEADLINE_MS` (5s) — the budget every
/// `Client::request` in this file gets, since none of them ever send a
/// deadline of their own. That ordering is what `rpc.rs`'s own
/// `an_oversized_action_timeout_loses_the_race` pins at the paused-clock
/// tier; here it costs real wall-clock time, so the value is kept small
/// deliberately rather than left at the 3s spec default.
///
/// The child reads (and discards) the action before falling silent, rather
/// than never touching fd 3 at all — a fixture that never reads still leaves
/// the message sitting in the socket's own kernel buffer, which times out
/// exactly the same way for a reason this test is not about. Reading first
/// is what makes the silence itself the fact under test.
#[tokio::test]
async fn a_trigger_against_a_silent_child_times_out_rather_than_hitting_the_rpc_deadline() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;

    let mut app = AppConfig::minimal("silent", "/bin/sh");
    app.interpreter = Some("none".to_string());
    app.channel = true;
    app.action_timeout = UpDuration::from_millis(500);
    app.args = vec![
        "-c".to_string(),
        "read -r _line <&3; while :; do sleep 1; done".to_string(),
    ];
    let started = client.request(Request::Start { apps: vec![app] }).await;
    let Response::Started(infos) = started.result.unwrap() else {
        panic!("expected started")
    };
    let id = infos[0].id;

    let triggered = client
        .request(Request::Trigger {
            selector: SelectorSpec::Id(id),
            action: "gc".to_string(),
            params: None,
        })
        .await;
    let Response::Triggered(rows) = triggered.result.unwrap() else {
        panic!("expected triggered")
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, id);
    assert_eq!(
        rows[0].outcome,
        ActionOutcome::TimedOut,
        "an app that never replies must produce a named TimedOut row, not a bare RPC error"
    );

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

/// Waits until `socket` is stale in the sense `bind_socket` means it —
/// nothing answers a connection there — failing at [`RECV_TIMEOUT`].
///
/// Polls rather than sleeping a fixed guess, and bounded rather than
/// unbounded (IR-39): the descriptor this waits on belongs to another
/// process, so there is no event to subscribe to, only a question to keep
/// asking until the deadline answers it.
///
/// Dropping a daemon's `UnixListener` stops THAT daemon accepting, but does
/// not on its own unbind the socket: a socket lives exactly as long as its
/// last descriptor, and `fork` hands the child a copy of every descriptor
/// open at that instant. A child parked between `fork` and `exec` therefore
/// keeps a dead daemon's listening socket bound, and connectable, for as
/// long as that window lasts — close-on-exec clears the copy, but not until
/// the `exec`. `bind_socket`'s stale-socket probe reads a socket that
/// answers as a live daemon and refuses the boot with `AlreadyRunning`, so
/// a reboot landing inside that window is turned away by a daemon that no
/// longer exists.
///
/// The window is reachable here because this tier runs many daemons inside
/// ONE process, concurrently with tests that spawn real children: a reboot
/// on one home can land inside an unrelated test's fork. Waiting for the
/// refusal makes the precondition a stale-socket reboot rests on asserted
/// rather than assumed. Not theorised — a child whose `pre_exec` sleeps
/// holds a just-crashed daemon's socket open by exactly this route, and a
/// reboot racing one is refused on every single run without this wait.
///
/// A daemon that is genuinely serving can never satisfy this wait: it
/// accepts, so every probe connects and the budget runs out. Only a socket
/// with nothing behind it refuses — because its last descriptor closed, or
/// because these probes filled a backlog nobody is draining, which some
/// Unixes report as a refusal too. Both are the answer `bind_socket` acts
/// on, which is what makes either an honest stopping point.
async fn await_stale_socket(socket: &std::path::Path) {
    let refused = tokio::time::timeout(RECV_TIMEOUT, async {
        // tokio's connector, not `std`'s blocking one, and that is
        // load-bearing rather than stylistic: nothing accepts on the socket
        // this waits out, so every probe leaves a connection queued, and a
        // full backlog is reported differently across Unixes — some refuse
        // it, some park the caller until a slot frees. A blocking `connect`
        // that parks holds the runtime thread inside a syscall no timer can
        // interrupt, and this fn would then outlive the very budget it
        // exists to enforce.
        while !matches!(
            UnixStream::connect(socket).await,
            Err(err) if matches!(err.kind(), ErrorKind::ConnectionRefused | ErrorKind::NotFound)
        ) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        refused.is_ok(),
        "{}: a crashed daemon's socket is still answering connections",
        socket.display()
    );
}

#[tokio::test]
async fn a_socket_left_behind_by_a_crash_does_not_block_the_next_boot() {
    let mut fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let socket = fixture.paths.socket.clone();

    // Simulate a crash: abort the run loop instead of asking for its
    // ordered teardown, so neither the socket file nor the pidfile is ever
    // unlinked. Awaiting the now-aborted handle (rather than a fixed sleep)
    // is what makes the DROP deterministic: it only resolves once the task
    // — and the `UnixListener` it owned — has actually finished dropping.
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
    // Dropping that listener is not the same as the socket going dead, and
    // the reboot below needs the second: see `await_stale_socket`'s own doc
    // for what else can hold a crashed daemon's socket open, and why
    // awaiting the aborted task does not by itself rule it out.
    await_stale_socket(&socket).await;

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

/// Serializes this file's two reload measurements against each other.
///
/// Each hands a FIXED port to a child that has to bind it twice over — the
/// instance being replaced and its replacement, at once — so the two cannot be
/// allowed to interleave, and `cargo test` runs the tests inside one binary in
/// parallel by default. This is `boot.rs`'s `SIGNAL_TEST_LOCK` again, for the
/// same class of reason its own doc records: a process-wide resource one test
/// owns silently reaching another and deciding its result. `tokio::sync::Mutex`
/// for that doc's reason too — the guard is held across `.await` points, where
/// clippy's `await_holding_lock` correctly denies a blocking guard.
///
/// It does not serialize against other test BINARIES, which cargo also runs
/// concurrently. Nothing else in the workspace has a child bind a port, and
/// each measurement takes a port the OS handed out moments earlier; the
/// residual risk is [`free_port`]'s own.
static RELOAD_PORT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// How long [`reuse_port_sheep`] holds a connection before answering it.
///
/// A reply is not instant because a handover's cost is mostly what happens to
/// work already in hand: at [`CONNECT_INTERVAL`] this keeps around fifteen
/// connections open at every instant, which is what an instance killed
/// mid-flight destroys and a draining one finishes.
const HOLD_MS: u64 = 60;

/// One new connection every 4ms for as long as a reload lasts.
///
/// A rate, not a wait — IR-39's no-sleeps rule is about waiting for a
/// condition by guessing how long it takes, and this is the load the
/// measurement is made under. Fast enough that the window between the drainee
/// emptying its accept queue and closing its listener is a real chance to lose
/// something, slow enough that a loss is never the fixture's queue overflowing.
const CONNECT_INTERVAL: Duration = Duration::from_millis(4);

/// How long one connection gets before it counts as lost. Two orders of
/// magnitude over [`HOLD_MS`]: an answer that is coming arrives in milliseconds
/// and this is slack for a loaded runner, not an expected duration.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);

/// The `AwaitReady` window a replacement gets, as `listen_timeout`.
///
/// The fixture signals no readiness, so this app is the `Heuristic` case: the
/// deadline elapsing IS the readiness verdict, and it is what holds the
/// drainee's kill ladder back until the replacement has had time to bind. Half
/// a second against a process that binds in single-digit milliseconds — the
/// margin is the point, since the measurement must not be a race between exec
/// and a stop signal.
const READY_WINDOW: UpDuration = UpDuration::from_millis(500);

/// The drain window a replaced instance gets, as `graceful_timeout` — and, for
/// an instance that will not take its stop signal, exactly how long it is
/// before `SIGKILL`. Short only because nothing here needs longer; the spec
/// default is 8s and a test that waited it out twice would pay it for nothing.
const DRAIN_WINDOW: UpDuration = UpDuration::from_millis(1_000);

/// Connections opened at once before a reload and again after it, to establish
/// which process owns the port at each end of the swap.
const BURST: usize = 10;

/// What one connection to the fixture got.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Attempt {
    /// Answered, by the process with this pid. The pid is the whole reason the
    /// fixture replies with one: it attributes every served connection to a
    /// process, so "the port answered" and "the process I think is serving
    /// answered" are separate claims here.
    Served(u32),
    /// Refused, reset, timed out, or closed with nothing on it — carrying the
    /// reason so a failure message names it. A connection ACCEPTED into a
    /// listener's backlog and never answered because that listener closed
    /// arrives here as an empty answer, not as a connect error: the handshake
    /// completed, the reset came later.
    Failed(String),
}

impl Attempt {
    /// Whether this attempt got no answer — the thing the measurement counts.
    fn failed(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

/// One connection: open it, read what the server says, classify the outcome.
async fn attempt(port: u16) -> Attempt {
    let exchange = tokio::time::timeout(ATTEMPT_TIMEOUT, async {
        let mut conn = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
        let mut answer = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut conn, &mut answer).await?;
        std::io::Result::Ok(answer)
    });
    match exchange.await {
        Err(_) => Attempt::Failed(format!("no answer inside {ATTEMPT_TIMEOUT:?}")),
        Ok(Err(error)) => Attempt::Failed(error.to_string()),
        Ok(Ok(answer)) => match answer.trim().parse() {
            Ok(pid) => Attempt::Served(pid),
            Err(_) => Attempt::Failed(format!("answered {answer:?}")),
        },
    }
}

/// Opens [`BURST`] connections at once and hands back what each got.
async fn burst(port: u16) -> Vec<Attempt> {
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..BURST {
        set.spawn(attempt(port));
    }
    let mut attempts = Vec::new();
    while let Some(outcome) = set.join_next().await {
        attempts.push(outcome.expect("an attempt cannot panic"));
    }
    attempts
}

/// A caller that keeps connecting for as long as a reload takes.
struct Hammer {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    task: tokio::task::JoinHandle<Vec<Attempt>>,
}

impl Hammer {
    /// Starts opening one connection every [`CONNECT_INTERVAL`].
    fn start(port: u16) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task = tokio::spawn({
            let stop = std::sync::Arc::clone(&stop);
            async move {
                let mut set = tokio::task::JoinSet::new();
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    set.spawn(attempt(port));
                    tokio::time::sleep(CONNECT_INTERVAL).await;
                }
                // Every connection already open is waited out rather than
                // abandoned: the ones in flight when the swap finishes are
                // precisely the ones an instance killed mid-answer loses, and
                // dropping them here would drop the measurement with them.
                let mut attempts = Vec::new();
                while let Some(outcome) = set.join_next().await {
                    attempts.push(outcome.expect("an attempt cannot panic"));
                }
                attempts
            }
        });
        Self { stop, task }
    }

    /// Stops connecting and reports every attempt made, in-flight ones
    /// included.
    async fn finish(self) -> Vec<Attempt> {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.task.await.expect("the hammer cannot panic")
    }
}

/// A one-line tally of a run of attempts, so a failure message names the
/// reasons instead of dumping several hundred outcomes.
fn tally(attempts: &[Attempt]) -> String {
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for attempt in attempts {
        let key = match attempt {
            Attempt::Served(pid) => format!("served by {pid}"),
            Attempt::Failed(reason) => reason.clone(),
        };
        *counts.entry(key).or_default() += 1;
    }
    counts
        .into_iter()
        .map(|(reason, count)| format!("{count}x {reason}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Waits for `pid` to be the process answering `port`, failing at
/// [`RECV_TIMEOUT`].
///
/// The bus cannot say when a sheep has bound: the daemon arms a readiness wait
/// for a `Channel` or `Probe` app only, so this app — which configures neither
/// — is `Online` from the moment it is spawned, exec included. Polling the port
/// is what can, and it is the condition the measurement depends on rather than
/// a proxy for it. A replacement is a different matter and needs no poll: a
/// reload gates readiness for every app, which is what holds the drainee's
/// ladder back (`spawn_replacement`'s own doc).
async fn await_serving(port: u16, pid: u32) {
    let serving = tokio::time::timeout(RECV_TIMEOUT, async {
        while attempt(port).await != Attempt::Served(pid) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        serving.is_ok(),
        "pid {pid} must be the process answering 127.0.0.1:{port}"
    );
}

/// A port with nothing on it: bind `:0`, read what the OS chose, release it.
///
/// The repo's existing idiom (`supervisor.rs`, `boot.rs` and `extras.rs` all do
/// this) and inherently a check-then-use — a stranger can take the port between
/// the release here and the fixture's own bind. That loss is loud rather than
/// quiet: the fixture panics with the bind error into its own stderr log and
/// never reaches `Online`, so the wait for it fails instead of the measurement
/// quietly measuring something else. What cannot happen is the port still being
/// held by this file's other measurement — [`RELOAD_PORT_LOCK`] covers that,
/// and each measurement takes a fresh port regardless.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("the OS must have a free loopback port")
        .local_addr()
        .expect("a bound listener has an address")
        .port()
}

/// The fixture server's binary — see `examples/reuse_port_sheep.rs` for what it
/// is and why it is an example.
///
/// Located rather than named: `env!("CARGO_BIN_EXE_<name>")` covers a package's
/// `[[bin]]` targets and nothing else. Cargo puts an example at
/// `<target>/<triple?>/<profile>/examples/<name>` and this test binary at the
/// sibling `.../deps/<name>-<hash>`, so it is two levels up and back down — a
/// shape that survives `--target` (the musl leg of CI runs these tests) and a
/// custom `CARGO_TARGET_DIR`, since both move the whole tree together.
fn reuse_port_sheep() -> std::path::PathBuf {
    let test_binary = std::env::current_exe().expect("a running test knows its own path");
    let path = test_binary
        .parent()
        .and_then(std::path::Path::parent)
        .expect("a test binary lives at <profile>/deps/<name>")
        .join("examples")
        .join("reuse_port_sheep");
    assert!(
        path.is_file(),
        "{} must exist: a plain `cargo test` builds the package's examples, so a \
         missing one means this test was run some way that does not",
        path.display()
    );
    path
}

/// Reloads one `reuse_port_sheep` while a caller connects continuously, and
/// hands back every attempt made between the request and the swap finishing.
///
/// Asserts what holds whatever the app does with its stop signal, and on every
/// platform: the swap completes, the replacement is what answers the port
/// afterwards, and the instance it replaced is gone. The counting is the
/// caller's, because that is the part the two behaviours disagree about.
async fn reload_under_load(name: &str, defiant: bool) -> Vec<Attempt> {
    let _port_guard = RELOAD_PORT_LOCK.lock().await;
    let port = free_port();

    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;
    let subscribed = client
        .request(Request::Subscribe {
            topics: vec!["process.*".to_string()],
        })
        .await;
    assert_eq!(subscribed.result.unwrap(), Response::Subscribed);

    let mut app = AppConfig::minimal(name, &reuse_port_sheep().display().to_string());
    app.interpreter = Some("none".to_string());
    app.env
        .insert("SHEEP_PORT_BASE".to_string(), port.to_string());
    app.env
        .insert("SHEEP_HOLD_MS".to_string(), HOLD_MS.to_string());
    if defiant {
        app.env.insert("SHEEP_DEFIANT".to_string(), "1".to_string());
    }
    app.listen_timeout = READY_WINDOW;
    app.graceful_timeout = DRAIN_WINDOW;
    // Teardown's ladder, not the reload's. A defiant replacement has to be
    // SIGKILLed at the end of the test as well, and the spec's 1.6s default
    // would be 1.6s of waiting for a process that is never going to answer.
    app.kill_timeout = DRAIN_WINDOW;
    // Nothing may respawn behind the measurement: a restart would put a third
    // process on this port and every count below would be about the wrong two.
    // The drainee's own exit is a claimed manual stop and would not restart
    // regardless — this covers everything else.
    app.autorestart = false;

    let started = client.request(Request::Start { apps: vec![app] }).await;
    let Response::Started(infos) = started.result.unwrap() else {
        panic!("expected started")
    };
    let drainee_id = infos[0].id;
    let drainee_pid = infos[0].pid.expect("a real spawn reports a real pid");
    client
        .await_process_event(drainee_id, ProcessEventKind::Online)
        .await;
    await_serving(port, drainee_pid).await;

    // The port answers, and it answers as the process this test thinks is on
    // it. Both halves are load-bearing: this is what rules out a stranger, a
    // leftover from the sibling measurement, or a client of this test's own
    // making being the reason any later attempt fails.
    let before = burst(port).await;
    assert_eq!(
        tally(&before),
        format!("{BURST}x served by {drainee_pid}"),
        "the sheep must own the port outright before its reload begins"
    );

    let hammer = Hammer::start(port);
    let accepted = client
        .request(Request::Reload {
            selector: SelectorSpec::Name(name.to_string()),
        })
        .await;
    let Response::Reloading(accepted) = accepted.result.unwrap() else {
        panic!("expected an accepted reload")
    };
    assert_eq!(accepted.len(), 1);

    // `Reloaded` is the one event that says a swap SUCCEEDED, and it carries
    // the replacement. Waiting for it — rather than for a duration — is what
    // makes the window measured below exactly the reload.
    let replacement = client
        .await_any_process_event(ProcessEventKind::Reloaded)
        .await;
    let during = hammer.finish().await;
    let replacement_pid = replacement.pid.expect("a replacement has a pid");
    assert_ne!(replacement_pid, drainee_pid);
    assert_reaped(i32::try_from(drainee_pid).unwrap()).await;

    // One row, the replacement's: the drainee's registration went with the
    // process rather than leaving a second entry in an instance slot its
    // replacement now holds.
    let listed = client.request(Request::ListFlock).await;
    let Response::Flock(flock) = listed.result.unwrap() else {
        panic!("expected flock")
    };
    assert_eq!(flock.len(), 1);
    assert_eq!(flock[0].id, replacement.id);
    // The status is the sharpest assertion in this function, and the one that
    // catches a swap committed before its replacement could prove anything —
    // see `a_reload_costs_a_draining_app_no_connections`'s second "fails if",
    // where that mutation costs no connections at all and shows up only here.
    assert_eq!(flock[0].status, ProcStatus::Online);

    // The replacement serves, on the same port, under the same instance slot —
    // the fixture derives its port from `SHEP_INSTANCE`, so a replacement put
    // in a different slot would be answering somewhere else entirely.
    let after = burst(port).await;
    assert_eq!(
        tally(&after),
        format!("{BURST}x served by {replacement_pid}"),
        "the replacement must own the port outright once the swap is done"
    );

    let dir = fixture.shutdown().await;
    assert_reaped(i32::try_from(replacement_pid).unwrap()).await;
    drop(dir);

    during
}

/// A reload of an app that drains costs a caller connecting continuously
/// nothing.
///
/// # What shep promises here, and what it does not
///
/// The overlap, not zero downtime. Mid-swap both instances are bound to the
/// same port and both are serving, which is the window an application needs in
/// order to hand over — but a listener's accept backlog is RESET when it
/// closes, so what is queued and not yet accepted is lost unless the app itself
/// drains inside `graceful_timeout`. Zero downtime is the application's
/// achievement; the window is shep's. This measures a cooperating app's side of
/// that bargain, `a_reload_costs_a_defiant_app_the_work_it_will_not_finish`
/// measures the other, and the gap between the two counts is the finding
/// neither of them states alone.
///
/// # Why the count is asserted on Linux only
///
/// Because the two platforms do not share the mechanism the count is about.
/// From this test's own runs (2026-08-10, ~95 connections across the reload):
///
/// - **Linux** load-balances new connections over every listener in the
///   `SO_REUSEPORT` group, so the instance being replaced keeps taking a share
///   of them right up until it closes — 47 to the drainee, 48 to its
///   replacement. Zero here says the drainee carried half the traffic through
///   the whole overlap and handed every connection over.
/// - **macOS** gives every new connection to the LAST socket to bind, so the
///   drainee stops receiving them the moment its replacement is up — 1 to the
///   drainee, 93 to its replacement. There is almost nothing left for a zero
///   here to be about, and the mutation below proves it: a drain that waits
///   nothing before `SIGKILL` still costs macOS zero connections.
///
/// So macOS asserts what it can still see, which is what `reload_under_load`
/// asserts for both: the swap completes, the replacement is what answers the
/// port afterwards, and the instance it replaced is gone.
///
/// # Fails if
///
/// **The drain stops waiting** (Linux). `LadderCap::of`'s `Drain` arm returning
/// `Duration::ZERO` — a ladder that signals and `SIGKILL`s in the same breath —
/// takes this to 5 lost of 89, every one of them a connection the drainee had
/// accepted and not yet answered. The same mutation on macOS: 0 lost of 94,
/// which is the platform statement above made concrete.
///
/// **The swap is committed before the replacement can serve** (both platforms),
/// which is a reload degenerating into a restart. Calling `begin_drain` from
/// `spawn_replacement`'s success arm instead of leaving it to the replacement's
/// readiness result reddens the flock check inside `reload_under_load` — the
/// swap reports `Reloaded` with its replacement still `Starting`. Measured, and
/// worth recording because the guess was wrong: it does NOT show up in the
/// count on either platform (0 lost of 11 on Linux, 0 of 13 on macOS). The
/// drainee's own drain outlasts its replacement's exec, so the port is never
/// actually unserved — the loss the mutation causes is to the meaning of
/// `Reloaded`, not to any connection.
///
/// Nothing else in the suite reaches either: the engine tier's runner has no
/// sockets, so there a swap that keeps its ordering and one that drops it
/// produce the same events.
#[tokio::test]
async fn a_reload_costs_a_draining_app_no_connections() {
    let during = reload_under_load("drainer", false).await;
    let failures = during.iter().filter(|attempt| attempt.failed()).count();
    // Printed on every platform, asserted on one: the count is the finding, and
    // it is what the sibling measurement's own count is worth reading against.
    println!(
        "draining app, {} attempts across the reload, {failures} lost: {}",
        during.len(),
        tally(&during)
    );
    assert!(
        during.len() > 20,
        "the reload must last long enough to be measured: {}",
        tally(&during)
    );
    #[cfg(target_os = "linux")]
    assert_eq!(
        failures,
        0,
        "an app that drains inside its graceful timeout must lose nothing: {}",
        tally(&during)
    );
}

/// A reload of an app that ignores its stop signal loses the caller work, and
/// shep completes the swap anyway.
///
/// The honest half of the pair. shep's contribution is the overlap; an instance
/// that will not stop accepting, finish what it has, and exit inside
/// `graceful_timeout` reaches the end of that window still holding work, and
/// `SIGKILL` takes the work with it. No supervisor can give that app zero
/// downtime, and a suite that shipped only the cooperating fixture would be
/// asserting a promise shep does not make.
///
/// # Why the count is asserted on Linux only
///
/// Because only Linux can produce it, for the reason the sibling test's doc
/// records: there the defiant instance is still being handed a share of every
/// new connection, and still holding some unanswered, at the moment `SIGKILL`
/// lands. Measured across five runs each — Linux 5, 7, 5, 7 and 8 lost of
/// ~260; macOS 0 of ~280, every run. On macOS the defiant instance is handed
/// nothing from the moment its replacement binds, so it is killed empty, and
/// asserting non-zero there would be asserting a bug that platform cannot have.
///
/// # Fails if
///
/// **The application cooperates** (Linux) — which is the whole point of
/// counting rather than asserting a boolean. Dropping `SHEEP_DEFIANT` from this
/// app's environment takes the same reload, under the same load, from 5 lost to
/// 0 lost of 95, with the drainee still carrying half the connections (50 to
/// it, 45 to its replacement). Nothing about shep changed.
///
/// **The ladder stops escalating** (both platforms). Removing `kill_process`'s
/// `SIGKILL` rung leaves a sheep that never exits, so no `Reloaded` ever
/// reaches the bus and the wait for it times out at 10s — the shared
/// assertions in `reload_under_load` are what catch that, and for this app
/// "the drainee is gone" can only mean the escalation happened.
#[tokio::test]
async fn a_reload_costs_a_defiant_app_the_work_it_will_not_finish() {
    let during = reload_under_load("defier", true).await;
    let failures = during.iter().filter(|attempt| attempt.failed()).count();
    println!(
        "defiant app, {} attempts across the reload, {failures} lost: {}",
        during.len(),
        tally(&during)
    );
    assert!(
        during.len() > 20,
        "the reload must last long enough to be measured: {}",
        tally(&during)
    );
    #[cfg(target_os = "linux")]
    assert!(
        failures > 0,
        "an app that will not drain must be seen to lose connections: {}",
        tally(&during)
    );
}
