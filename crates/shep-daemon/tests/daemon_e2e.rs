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

use shep_daemon::boot::{BootError, BootOptions, boot};
use shep_daemon::rpc::RpcContext;
use shep_daemon::tokio_runner::TokioRunner;

const RECV_TIMEOUT: Duration = Duration::from_secs(10);

/// A booted daemon on its own `$SHEP_HOME`, with its run loop spawned.
struct Fixture {
    dir: tempfile::TempDir,
    paths: ShepPaths,
    ctx: RpcContext,
    run: tokio::task::JoinHandle<Result<(), BootError>>,
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
            dir,
            paths,
            ctx,
            run,
        }
    }

    async fn connect(&self) -> Client {
        let stream = UnixStream::connect(&self.paths.socket).await.unwrap();
        let mut client = Client {
            frames: Framed::new(stream, codec()),
            next_id: 1,
            hello_ack: None,
            pending: std::collections::VecDeque::new(),
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
    async fn shutdown(self) -> tempfile::TempDir {
        self.ctx.shutdown();
        tokio::time::timeout(RECV_TIMEOUT, self.run)
            .await
            .expect("teardown must not hang")
            .unwrap()
            .unwrap();
        self.dir
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

    let line = loop {
        if let ServerFrame::Event(BusEvent::LogOut { id: event_id, line }) =
            client.next_frame().await
            && event_id == id
        {
            break line;
        }
    };
    assert_eq!(line, "hello-flock");

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
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
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
    tokio::time::timeout(RECV_TIMEOUT, fixture.run)
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

    // Neither child may survive teardown: poll kill(pid, None) for ESRCH
    // (no such process) instead of sleeping a fixed guess.
    for pid in pids {
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

    // Engine unreachable: a fresh connect on the now-unlinked socket path
    // must fail, not hang or succeed against a daemon that never really left.
    assert!(
        UnixStream::connect(&socket).await.is_err(),
        "the daemon must not still be answering after KillDaemon"
    );
}

#[tokio::test]
async fn a_socket_left_behind_by_a_crash_does_not_block_the_next_boot() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let socket = fixture.paths.socket.clone();

    // Simulate a crash: abort the run loop instead of asking for its
    // ordered teardown, so neither the socket file nor the pidfile is ever
    // unlinked. Awaiting the now-aborted handle (rather than a fixed sleep)
    // is what makes this deterministic: it only resolves once the task —
    // and the `UnixListener` it owned — has actually finished dropping, so
    // the reboot below's stale-socket probe can never race a listener that
    // hasn't finished closing yet.
    fixture.run.abort();
    let outcome = fixture.run.await;
    assert!(
        outcome.is_err_and(|err| err.is_cancelled()),
        "the run task must have been cancelled, not completed on its own"
    );
    assert!(
        socket.exists(),
        "sanity: a crash leaves the socket file behind"
    );

    // Same `$SHEP_HOME`: `dir` is moved out of `fixture` here rather than
    // dropped alongside it, so the directory (and the leftover socket file
    // inside it) survives into the reboot.
    let rebooted = Fixture::boot(fixture.dir, false).await;
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
