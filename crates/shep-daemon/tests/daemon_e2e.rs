//! Real-daemon integration tier: boots shep-daemon on a temp `$SHEP_HOME`,
//! talks to it over the control socket with shep-core's own codec, and
//! drives real child processes.
//!
//! Real time throughout: a paused clock's auto-advance would expire timeouts
//! before IO wakeups arrive.

// Many cases here are `#[cfg(unix)]`, so on Windows those items are unreached.
#![cfg_attr(windows, allow(dead_code))]
// And so are the imports only those cases use.
#![cfg_attr(windows, allow(unused_imports))]

use std::io::ErrorKind;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use shep_core::transport::{self, ClientStream};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use shep_core::config::{AppConfig, ProbeConfig, ProbeKind};
use shep_core::paths::ShepPaths;
use shep_core::protocol::{
    ActionOutcome, BusEvent, ChildMessage, Envelope, Hello, HelloAck, HelloReply, LineOutcome,
    PROTOCOL_VERSION, ProcessEventKind, ProcessInfo, Reply, Request, Response, RpcErrorCode,
    SelectorSpec, ServerFrame, codec, decode_frame, encode_frame,
};
use shep_core::status::ProcStatus;
use shep_core::values::UpDuration;

use shep_daemon::boot::{BootError, BootOptions, DIR_MODE, boot};
use shep_daemon::rpc::RpcContext;
use shep_daemon::tokio_runner::TokioRunner;

const RECV_TIMEOUT: Duration = Duration::from_secs(10);

/// A booted daemon on its own `$SHEP_HOME`, with its run loop spawned.
///
/// `run`/`dir` are `Option`-wrapped so this type can carry a [`Drop`] impl:
/// a field cannot be moved out of a value whose type implements `Drop`.
struct Fixture {
    dir: Option<tempfile::TempDir>,
    paths: ShepPaths,
    ctx: RpcContext,
    run: Option<tokio::task::JoinHandle<Result<(), BootError>>>,
    // Real OS pids this fixture must reap on the panic path.
    spawned: std::sync::Arc<std::sync::Mutex<Vec<i32>>>,
}

impl Fixture {
    async fn boot(dir: tempfile::TempDir, restore: bool) -> Self {
        // $SHEP_HOME is the tempdir root itself: `sun_path` caps the socket
        // path at 104 bytes on macOS, and macOS temp paths are already long.
        let home = dir.path().to_path_buf();
        let paths = ShepPaths::resolve(
            &|key| (key == "SHEP_HOME").then(|| home.display().to_string()),
            std::path::Path::new("/nonexistent"),
        );
        // Bounded: `boot` binds the control address, which on Windows is a
        // machine-global pipe name rather than a path under `dir`.
        let daemon = tokio::time::timeout(
            RECV_TIMEOUT,
            boot(
                TokioRunner::new(),
                paths.clone(),
                BootOptions {
                    restore,
                    ..BootOptions::default()
                },
            ),
        )
        .await
        .expect("boot must not hang: the control address is already held")
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
        // `transport::connect` retries ERROR_PIPE_BUSY forever, and is safe
        // only because every caller bounds it.
        let stream = tokio::time::timeout(RECV_TIMEOUT, transport::connect(&self.paths.socket))
            .await
            .expect("connect must not hang: the pipe stayed busy")
            .unwrap();
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
                dog_name: None,
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

/// Last-resort net for a test that panics before [`Fixture::shutdown`].
impl Drop for Fixture {
    /// Stops the daemon a panicking test never shut down, then reaps what a
    /// unix sheep leaves behind.
    ///
    /// `shutdown()` on the context alone, not a join: this runs during an
    /// unwind and must not block. A `current_thread` runtime unwinding stops
    /// polling `run`, so its kill ladder never finishes and the pids are
    /// signalled here, by process group to reach a `sleep 1` grandchild.
    ///
    /// Unix only: a Windows sheep cannot leave its job object.
    fn drop(&mut self) {
        // `None` only once `shutdown()` has taken it: the panic path.
        if self.run.is_some() {
            self.ctx.shutdown();
        }
        #[cfg(unix)]
        {
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
}

/// A handshaken connection to a booted [`Fixture`], speaking shep-core's own
/// length-delimited/JSON codec directly rather than through a client crate.
struct Client {
    frames: Framed<ClientStream, LengthDelimitedCodec>,
    next_id: u64,
    hello_ack: Option<HelloAck>,
    // Frames the current call was not looking for, in arrival order. A bus
    // event is emitted before the reply to the command that caused it, so
    // discarding one would hang a later `await_process_event`.
    pending: std::collections::VecDeque<ServerFrame>,
    // Shared with the owning `Fixture`.
    spawned: std::sync::Arc<std::sync::Mutex<Vec<i32>>>,
}

impl Client {
    /// The daemon's handshake answer, set by [`Fixture::connect`].
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
    /// hanging forever.
    async fn recv_as<T: DeserializeOwned>(&mut self) -> T {
        let frame = tokio::time::timeout(RECV_TIMEOUT, self.frames.next())
            .await
            .expect("timed out waiting for a frame")
            .expect("connection closed early")
            .unwrap();
        decode_frame(&frame).unwrap()
    }

    /// The next frame of any kind: whatever an earlier call read but didn't
    /// consume, oldest first, else the next one off the wire.
    ///
    /// Records every process event's pid: a reload's replacement can be
    /// orphaned by a panic before any reply carries it.
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
    /// re-queueing any bus events that arrive in between for a later call.
    async fn request(&mut self, body: Request) -> Reply {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&Envelope {
            id,
            deadline_ms: None,
            body,
        })
        .await;
        // Bounded: a daemon that never answers must fail by name, not hang.
        let mut skipped = Vec::new();
        let reply = tokio::time::timeout(RECV_TIMEOUT, async {
            loop {
                match self.next_frame().await {
                    ServerFrame::Reply(reply) if reply.id == id => break reply,
                    other => skipped.push(other),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for a reply to request {id}"));
        requeue(&mut self.pending, skipped);
        track_spawned(&self.spawned, &reply);
        reply
    }

    /// Sends a hand-built request body, past every validating newtype on
    /// [`Request`]. `None` when the daemon ended the connection instead,
    /// which is what it does with a body it cannot decode.
    ///
    /// Reads the wire directly: [`Self::next_frame`] panics on a closed
    /// connection, one of the two answers wanted here.
    async fn request_raw(&mut self, body: serde_json::Value) -> Option<Reply> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&serde_json::json!({
            "id": id,
            "deadline_ms": serde_json::Value::Null,
            "body": body,
        }))
        .await;
        let mut skipped = Vec::new();
        let reply = loop {
            let frame = tokio::time::timeout(RECV_TIMEOUT, self.frames.next())
                .await
                .expect("timed out waiting for a frame");
            let Some(frame) = frame else {
                break None; // the daemon closed on us: a refusal, not a hang
            };
            match decode_frame(&frame.unwrap()).unwrap() {
                ServerFrame::Reply(reply) if reply.id == id => break Some(reply),
                other => skipped.push(other),
            }
        };
        requeue(&mut self.pending, skipped);
        reply
    }

    /// Reads frames until a `Process` event of `kind` for `id` arrives,
    /// re-queueing everything else.
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

    /// Reads frames until a `Process` event of one of `kinds` arrives, and
    /// answers with which one.
    ///
    /// Stops at the first match, for a case whose subject is the order.
    async fn next_process_event_of(
        &mut self,
        kinds: &[ProcessEventKind],
    ) -> (ProcessEventKind, ProcessInfo) {
        let mut skipped = Vec::new();
        let found = loop {
            let frame = self.next_frame().await;
            if let ServerFrame::Event(BusEvent::Process { event, info, .. }) = &frame
                && kinds.contains(event)
            {
                break (*event, info.clone());
            }
            skipped.push(frame);
        };
        requeue(&mut self.pending, skipped);
        found
    }

    /// Reads frames until a `Process` event of `kind` arrives for any sheep,
    /// re-queueing everything else.
    ///
    /// For a reload's replacement, whose fresh id first reaches the client on
    /// the event itself.
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
    /// everything else.
    ///
    /// One overall [`RECV_TIMEOUT`], not `recv_as`'s per-frame one: a daemon
    /// emitting other frames forever must not spin this loop.
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

/// The interpreter a fixture's inline script is written for, and the flag
/// that makes it read one: `/bin/sh -c` on unix, `cmd /C` on Windows.
fn shell() -> (&'static str, &'static str) {
    #[cfg(unix)]
    {
        ("/bin/sh", "-c")
    }
    #[cfg(windows)]
    {
        ("cmd", "/C")
    }
}

/// An [`AppConfig`] running `script` under [`shell`].
///
/// `interpreter = "none"`: the script is already written for a shell, so shep
/// must not also resolve one from the program's extension.
fn shell_app(name: &str, script: String) -> AppConfig {
    let (program, flag) = shell();
    let mut app = AppConfig::minimal(name, program);
    app.interpreter = Some("none".to_string());
    app.args = vec![flag.to_string(), script];
    app
}

/// A sheep that stays up until something stops it.
///
/// `ping` on Windows: `timeout.exe` refuses to run with stdin redirected,
/// which every sheep's is, and `cmd` has no `sleep`.
fn forever_app(name: &str) -> AppConfig {
    #[cfg(unix)]
    let script = "while :; do sleep 1; done".to_string();
    #[cfg(windows)]
    let script = "ping -n 9999 127.0.0.1 >nul".to_string();
    shell_app(name, script)
}

/// A sheep that writes `line` to stdout and then stays up long enough to be
/// observed.
fn announce_app(name: &str, line: &str) -> AppConfig {
    #[cfg(unix)]
    let script = format!("echo {line}; sleep 5");
    #[cfg(windows)]
    let script = format!("echo {line}& ping -n 6 127.0.0.1 >nul");
    shell_app(name, script)
}

/// A sheep that sends one `ready` on the shepherd channel, then stays up.
///
/// fd 3 on unix, so a `>&3` redirect is the whole contract. Windows has no
/// fd-3 inheritance: the daemon exports the pipe name as
/// `%SHEP_CHANNEL_PIPE%`. The caller still sets `wait_ready`, which is what
/// makes `assemble` open the channel.
fn ready_app(name: &str, dir: &std::path::Path) -> AppConfig {
    #[cfg(unix)]
    {
        let _ = dir;
        shell_app(
            name,
            r#"printf '{"kind":"ready"}
' >&3; while :; do sleep 1; done"#
                .to_string(),
        )
    }
    // A `.cmd` file, not `cmd /C <script>`: `std::process::Command` escapes an
    // argument's inner quotes as `\"`, which `cmd.exe` takes literally, so the
    // redirect target arrives malformed.
    #[cfg(windows)]
    {
        let script = dir.join(format!("{name}-ready.cmd"));
        let mut body = String::new();
        for line in [
            "@echo off",
            "(echo {\"kind\":\"ready\"}) > \"%SHEP_CHANNEL_PIPE%\"",
            "ping -n 9999 127.0.0.1 >nul",
        ] {
            body.push_str(line);
            body.push('\r');
            body.push('\n');
        }
        std::fs::write(&script, body).unwrap();
        let mut app = AppConfig::minimal(name, &script.display().to_string());
        app.interpreter = Some("none".to_string());
        app
    }
}

/// A sheep that writes `before`, waits for `marker` to appear, writes
/// `after`, then stays up. The fixture the log-rotation cases drive.
fn gated_announce_app(name: &str, marker: &std::path::Path) -> AppConfig {
    #[cfg(unix)]
    {
        let script = format!(
            "echo before; while [ ! -f {} ]; do sleep 1; done; echo after; sleep 5",
            marker.display()
        );
        let mut app = shell_app(name, script);
        app.autorestart = false;
        app
    }
    // A batch file, not a `cmd /C` one-liner: `goto` needs labels, which exist
    // only in a file, so an inline loop does not loop. `ping -n 2` is the
    // sleep, for `forever_app`'s reason.
    #[cfg(windows)]
    {
        const CRLF: &str = "\r\n";
        let script = marker.with_file_name("gated.cmd");
        let body = [
            "@echo off".to_string(),
            "echo before".to_string(),
            ":wait".to_string(),
            format!("if exist \"{}\" goto ready", marker.display()),
            "ping -n 2 127.0.0.1 >nul".to_string(),
            "goto wait".to_string(),
            ":ready".to_string(),
            "echo after".to_string(),
            "ping -n 6 127.0.0.1 >nul".to_string(),
            String::new(),
        ];
        std::fs::write(&script, body.join(CRLF))
            .expect("the gated fixture script must be writable");
        let mut app = shell_app(name, script.display().to_string());
        // The script exits and `autorestart` is on by default, so without this
        // the log gains a second `before` and `after`.
        app.autorestart = false;
        app
    }
}

/// Records every live pid a reply's `ProcessInfo`s carry, for `Fixture`'s
/// panic-path cleanup.
///
/// Every variant that can carry a pid, not just `Started`: a muster restore's
/// fresh pids are only ever seen via a post-reboot `ListFlock`.
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

/// Records one `ProcessInfo`'s pid, if it has one.
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

/// Restores frames a call read but didn't want to the front of `pending`, in
/// arrival order, so [`Client::next_frame`] sees them before the wire.
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

    // Subscribe before starting: the bus delivers from the moment you join.
    let subscribed = client
        .request(Request::Subscribe {
            topics: vec!["process.*".to_string()],
        })
        .await;
    assert_eq!(subscribed.result.unwrap(), Response::Subscribed);

    let app = forever_app("sleeper");
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
    // The reply is deferred until the kill ladder finished: terminal.
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

    let app = announce_app("chatty", "hello-flock");
    let started = client.request(Request::Start { apps: vec![app] }).await;
    let Response::Started(infos) = started.result.unwrap() else {
        panic!("expected started")
    };
    let id = infos[0].id;

    let line = client.await_log_line(id).await;
    assert_eq!(line, "hello-flock");

    fixture.shutdown().await;
}

/// A log file's contents with the daemon's per-line timestamp taken off.
///
/// A missing or unreadable file reads as the empty string.
fn unstamped_file(path: &std::path::Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut out = String::new();
    for line in text.lines() {
        out.push_str(shep_core::logstamp::strip(line));
        out.push('\n');
    }
    out
}

/// Waits for `path` to hold exactly `expected`, failing at [`RECV_TIMEOUT`].
///
/// Polls: a line seen on the bus has had its write issued, not completed,
/// since `tokio::fs` dispatches the real `write(2)` to the blocking pool.
async fn await_file_contents(path: &std::path::Path, expected: &str) {
    let settled = tokio::time::timeout(RECV_TIMEOUT, async {
        while unstamped_file(path) != expected {
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

/// Both halves are asserted: a pump that opened a second handle without
/// dropping the first would grow the new file too, and only the archive
/// standing still rules that out.
#[tokio::test]
async fn reopen_moves_a_running_sheeps_log_onto_the_recreated_path() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;

    // Subscribe before starting: a connection gets no events until it does.
    let subscribed = client
        .request(Request::Subscribe {
            topics: vec!["log.*".to_string()],
        })
        .await;
    assert_eq!(subscribed.result.unwrap(), Response::Subscribed);

    // The marker makes "after the reopen" a fact rather than a timing bet.
    let marker = fixture.paths.home.join("go");
    let app = gated_announce_app("rotator", &marker);
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

    // The reply is the barrier: the pump has flushed the old handle and
    // opened the path again, so neither of these polls.
    assert_eq!(unstamped_file(&out_file), "");
    assert_eq!(unstamped_file(&archive), "before\n");

    std::fs::write(&marker, "").unwrap();
    assert_eq!(client.await_log_line(id).await, "after");
    await_file_contents(&out_file, "after\n").await;
    assert_eq!(
        unstamped_file(&archive),
        "before\n",
        "the renamed file must stop growing the moment the handle is swapped"
    );

    fixture.shutdown().await;
}

#[cfg(unix)]
/// The case a rotator that moves the directory aside rather than the files
/// produces.
///
/// Under `umask 0o077` a plain `create_dir_all` lands `0o700` unaided and both
/// implementations look alike here; narrowing that would take a process-wide
/// umask, which is `unsafe` and leaks into every other case in this binary.
#[tokio::test]
async fn reopen_recreates_a_removed_log_directory_owner_only() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;

    // The marker lives beside the log directory, not inside it: removing that
    // directory must not disturb it.
    let marker = fixture.paths.home.join("go");
    let app = gated_announce_app("rotator", &marker);
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

    // The whole directory, not the file: `mkdir`'s mode governs only the
    // directories a call creates.
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

    // The reply is the barrier: both handles are open on the recreated path,
    // so the next line says the directory is usable and not merely present.
    std::fs::write(&marker, "").unwrap();
    await_file_contents(&out_file, "after\n").await;

    fixture.shutdown().await;
}

/// What the flush case writes at the live log path after renaming the real
/// one away, standing in for the file a `create`-mode rotator leaves behind.
const STRAY_CONTENT: &str = "what the recreated log holds\n";

/// [`STRAY_CONTENT`] keeps the recorded path and the pump's inode
/// distinguishable: without it the live path would simply be missing and the
/// truncate would be the documented no-op.
#[tokio::test]
async fn flush_empties_the_recorded_path_and_leaves_a_renamed_archive_alone() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;

    // Subscribe before starting: a connection gets no events until it does.
    let subscribed = client
        .request(Request::Subscribe {
            topics: vec!["log.*".to_string()],
        })
        .await;
    assert_eq!(subscribed.result.unwrap(), Response::Subscribed);

    let app = announce_app("noisy", "before");
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

    // From here the pump's handle and the recorded path name different files.
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

    // The reply is the barrier: every matched pump has answered, so neither
    // of these polls.
    assert_eq!(
        unstamped_file(&out_file),
        "",
        "the recorded path is what a flush empties"
    );
    assert_eq!(
        unstamped_file(&archive),
        "before\n",
        "the renamed file is not the daemon's to empty — a flush that chased \
         the pump's inode would have emptied this one instead"
    );

    fixture.shutdown().await;
}

/// How long this test waits for the gated sheep's `Online`. A small fraction
/// of the `listen_timeout` below: the gap between the two is the assertion.
const READY_DEADLINE: Duration = Duration::from_secs(5);

/// The only case that drives a real child's fd 3 through `run_sheep`'s
/// `ChildMessage::Ready -> Msg::Ready` forward.
///
/// `listen_timeout` is two orders of magnitude past [`READY_DEADLINE`]: an
/// elapsed readiness deadline brings the sheep online rather than failing it,
/// so only an early `Online` tells a forwarded ready message from an expired
/// one.
#[tokio::test]
async fn a_wait_ready_sheep_goes_online_on_its_own_channel_message() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;

    // Subscribe before starting: the bus delivers from the moment you join.
    let subscribed = client
        .request(Request::Subscribe {
            topics: vec!["process.*".to_string()],
        })
        .await;
    assert_eq!(subscribed.result.unwrap(), Response::Subscribed);

    let mut app = ready_app("greeter", &fixture.paths.home);
    // `wait_ready` both arms the gate and makes `assemble` open the channel.
    app.wait_ready = true;
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

/// Two round trips, not one: a single reply can land by winning a
/// spawn-timing race. The child echoes a counter, so the two are
/// distinguishable.
///
/// A successful `to_child.send()` is not delivery: the first send after a
/// child has died is accepted and discarded. The `Replied` row is the proof.
// `cfg(unix)` because its fixture is a `/bin/sh` script.
#[cfg(unix)]
#[tokio::test]
async fn a_triggered_action_reaches_a_real_child_and_answers_it_twice() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;

    let mut app = AppConfig::minimal("responder", "/bin/sh");
    app.interpreter = Some("none".to_string());
    // `channel` is what opens fd 3 here; this app gates no readiness on it.
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

// `cfg(unix)` because its fixture is a `/bin/sh` script.
#[cfg(unix)]
#[tokio::test]
async fn a_line_written_to_a_real_sheeps_stdin_comes_back_on_its_stdout() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;

    // Subscribe before starting: the bus delivers from the moment you join.
    let subscribed = client
        .request(Request::Subscribe {
            topics: vec!["log.*".to_string()],
        })
        .await;
    assert_eq!(subscribed.result.unwrap(), Response::Subscribed);

    let mut app = AppConfig::minimal("echoer", "/bin/sh");
    app.interpreter = Some("none".to_string());
    app.args = vec![
        "-c".to_string(),
        "while IFS= read -r line; do echo \"got $line\"; done".to_string(),
    ];
    app.stdin = true;

    let started = client.request(Request::Start { apps: vec![app] }).await;
    let Response::Started(infos) = started.result.unwrap() else {
        panic!("expected started")
    };
    let id = infos[0].id;

    let reply = client
        .request(Request::SendLine {
            selector: SelectorSpec::Name("echoer".to_string()),
            line: "ping".to_string(),
        })
        .await;
    let Response::SentLine(rows) = reply.result.unwrap() else {
        panic!("expected sent line")
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].outcome, LineOutcome::Sent);

    // `Sent` only claims the bytes reached the pipe; this proves a read.
    let echoed = client.await_log_line(id).await;
    assert_eq!(echoed, "got ping");

    fixture.shutdown().await;
}

// `cfg(unix)` because its fixture is a `/bin/sh` script.
#[cfg(unix)]
#[tokio::test]
async fn a_childs_metric_reaches_a_channel_subscriber_over_the_socket() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;

    // Subscribe before starting: the bus delivers from the moment you join.
    let subscribed = client
        .request(Request::Subscribe {
            topics: vec!["channel.*".to_string()],
        })
        .await;
    assert_eq!(subscribed.result.unwrap(), Response::Subscribed);

    let mut app = AppConfig::minimal("chatty", "/bin/sh");
    app.interpreter = Some("none".to_string());
    app.channel = true;
    // Sleeps after the metric: the sheep must outlive the assertion.
    app.args = vec![
        "-c".to_string(),
        r#"printf '{"kind":"metric","name":"rps","value":42}\n' >&3; sleep 30"#.to_string(),
    ];
    client.request(Request::Start { apps: vec![app] }).await;

    // Bounded: a subscriber that never receives must fail, not hang.
    let frame = tokio::time::timeout(RECV_TIMEOUT, async {
        loop {
            if let ServerFrame::Event(BusEvent::Channel { message, .. }) = client.next_frame().await
            {
                break message;
            }
        }
    })
    .await
    .expect("no channel.* frame within the timeout");

    match frame {
        ChildMessage::Metric { name, value } => {
            assert_eq!(name, "rps");
            assert!((value - 42.0).abs() < f64::EPSILON, "{value}");
        }
        other => panic!("subscribed to channel.*, received {other:?}"),
    }

    fixture.shutdown().await;
}

/// `AppConfig::minimal` leaves `channel`, `wait_ready` and
/// `shutdown_with_message` false, the three `assemble()` ors together to
/// decide whether a sheep gets fd 3.
#[tokio::test]
async fn a_trigger_against_a_channelless_sheep_names_the_missing_channel() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;

    let app = forever_app("mute");
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

/// `action_timeout` is set under both [`RECV_TIMEOUT`] and `rpc.rs`'s
/// `DEFAULT_DEADLINE_MS` (5s), the budget every `Client::request` here gets.
///
/// The child reads the action before falling silent: a fixture that never
/// reads leaves the message in the kernel buffer, which times out the same
/// way for a different reason.
// `cfg(unix)` because its fixture is a `/bin/sh` script.
#[cfg(unix)]
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

    // By hand: `Fixture::connect` always sends a matching protocol.
    let stream = transport::connect(&fixture.paths.socket).await.unwrap();
    let mut frames = Framed::new(stream, codec());
    frames
        .send(
            encode_frame(&Hello {
                client_version: "9.9.9".to_string(),
                protocol: PROTOCOL_VERSION + 1,
                dog_name: None,
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

#[cfg(unix)]
// `cfg(unix)` because its fixture is a `/bin/sh` script.
#[cfg(unix)]
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

    // Reaped, not merely signalled: `kill(pid, None)` still returns `Ok` for a
    // zombie, so only ESRCH proves the daemon's own `wait()` ran. The `sleep 1`
    // grandchildren are out of reach here; `real_runner.rs` covers them.
    for pid in pids {
        assert_reaped(pid).await;
    }

    // A fresh connect on the unlinked path must fail, not hang.
    assert!(
        transport::connect(&socket).await.is_err(),
        "the daemon must not still be answering after KillDaemon"
    );
}

#[cfg(unix)]
/// Polls `kill(pid, None)` for ESRCH, no such process, rather than sleeping a
/// fixed guess.
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

/// Waits until nothing answers a connection at `socket`, failing at
/// [`RECV_TIMEOUT`].
///
/// Dropping a `UnixListener` does not unbind the socket: it lives as long as
/// its last descriptor, and a child parked between `fork` and `exec` holds a
/// copy until close-on-exec clears it. `bind_socket` reads such a socket as a
/// live daemon and refuses the boot with `AlreadyRunning`.
async fn await_stale_socket(socket: &std::path::Path) {
    let refused = tokio::time::timeout(RECV_TIMEOUT, async {
        // tokio's connector, not `std`'s: a full backlog parks the caller on
        // some Unixes, inside a syscall no timer can interrupt.
        while !matches!(
            transport::connect(socket).await,
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

// `cfg(unix)`: a leftover socket file, which a named pipe never leaves.
#[cfg(unix)]
#[tokio::test]
async fn a_socket_left_behind_by_a_crash_does_not_block_the_next_boot() {
    let mut fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let socket = fixture.paths.socket.clone();

    // Simulate a crash: aborting the run loop unlinks neither the socket file
    // nor the pidfile. Awaiting the handle resolves once the task, and the
    // `UnixListener` it owned, has finished dropping.
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
    // Dropping that listener is not the socket going dead; the reboot needs
    // the second.
    await_stale_socket(&socket).await;

    // Same `$SHEP_HOME`: taking `dir` out of `fixture` keeps the leftover
    // socket file alive into the reboot.
    let dir = fixture.dir.take().expect("dir is only ever taken once");
    let rebooted = Fixture::boot(dir, false).await;
    let mut client = rebooted.connect().await;
    let pong = client.request(Request::Ping).await;
    assert_eq!(pong.result.unwrap(), Response::Pong);

    rebooted.shutdown().await;
}

#[cfg(unix)]
// `cfg(unix)` because its fixture is a `/bin/sh` script.
#[cfg(unix)]
#[tokio::test]
async fn muster_restores_the_flock_across_a_daemon_lifetime() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;

    let alpha = forever_app("alpha");
    let mut beta = forever_app("beta");
    beta.instances = 2;
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

    // Reaped, not merely recorded in the roll: a stale pid the OS had not
    // reused would pass the fresh-pid assertion for the wrong reason.
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

/// Prepends `dir` to `PATH` for one test, restoring the original on drop.
///
/// Prepending, never replacing: a concurrently spawned sleeper's `/bin/sh`
/// has to keep finding `sleep` while this guard is active.
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
        // SAFETY: `set_var`'s hazard is a concurrent raw `getenv`. Every read
        // of `PATH` in this binary goes through `std::env::var`, which std
        // serializes against `set_var`/`remove_var`.
        unsafe { std::env::set_var("PATH", combined) };
        Self { original }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        match &self.original {
            // SAFETY: every `PATH` read in this binary goes through
            // `std::env::var`, which std serializes against `set_var`.
            Some(value) => unsafe { std::env::set_var("PATH", value) },
            // SAFETY: every `PATH` read in this binary goes through
            // `std::env::var`, which std serializes against `remove_var`.
            None => unsafe { std::env::remove_var("PATH") },
        }
    }
}

#[cfg(unix)]
// `cfg(unix)` because its fixture is a `/bin/sh` script.
#[cfg(unix)]
#[tokio::test]
async fn a_bare_interpreter_resolves_via_the_inherited_path() {
    // A throwaway-tempdir shim, not a bare `"sh"`: `execvp` falls back to
    // `_PATH_DEFPATH` when PATH is absent from the child's env, so a bare name
    // would resolve even with `base_env()`'s seeding reverted.
    use std::os::unix::fs::PermissionsExt as _;

    let shim_home = tempfile::tempdir().unwrap();
    let shim_dir = shim_home.path().join("bin");
    std::fs::create_dir_all(&shim_dir).unwrap();
    let shim_path = shim_dir.join("shep-test-interp");
    std::fs::write(&shim_path, "#!/bin/sh\necho shep-bare-interpreter-ok\n").unwrap();
    let mut perms = std::fs::metadata(&shim_path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&shim_path, perms).unwrap();

    // The only test in this binary that mutates PATH.
    let _path_guard = PathGuard::prepend(&shim_dir);

    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;
    // Subscribe before starting: a connection gets no events until it does.
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

    // A failed exec lands the sheep in Errored, so Online is the assertion.
    let online = client
        .await_process_event(id, ProcessEventKind::Online)
        .await;
    assert_eq!(online.status, ProcStatus::Online);

    fixture.shutdown().await;
}

/// Serializes this file's reload measurements: each hands a fixed port to a
/// child that binds it twice over, so the two cannot interleave.
///
/// `tokio::sync::Mutex` because the guard is held across `.await`, where
/// clippy's `await_holding_lock` denies a blocking guard. It does not
/// serialize against other test binaries, which cargo also runs concurrently.
static RELOAD_PORT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// How long [`reuse_port_sheep`] holds a connection before answering it.
///
/// At [`CONNECT_INTERVAL`] this keeps around fifteen connections open at every
/// instant, which an instance killed mid-flight destroys.
const HOLD_MS: u64 = 60;

/// One new connection every 4ms for as long as a reload lasts.
///
/// Fast enough that the window between the drainee emptying its accept queue
/// and closing its listener is a real chance to lose something, slow enough
/// that a loss is never the fixture's queue overflowing.
const CONNECT_INTERVAL: Duration = Duration::from_millis(4);

/// How long one connection gets before it counts as lost. Two orders of
/// magnitude over [`HOLD_MS`]: slack for a loaded runner, not an expected
/// duration.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);

/// The `AwaitReady` window a replacement gets, as `listen_timeout`.
///
/// The fixture signals no readiness, so this is the `Heuristic` case: the
/// deadline elapsing is the verdict, and it holds the drainee's kill ladder
/// back until the replacement has bound. Half a second against a process that
/// binds in single-digit milliseconds.
const READY_WINDOW: UpDuration = UpDuration::from_millis(500);

/// The drain window a replaced instance gets, as `graceful_timeout`, and for
/// an instance that will not take its stop signal, how long it is before
/// `SIGKILL`. Short because nothing here needs longer; the spec default is 8s.
const DRAIN_WINDOW: UpDuration = UpDuration::from_millis(1_000);

/// Connections opened at once before a reload and again after it, to establish
/// which process owns the port at each end of the swap.
const BURST: usize = 10;

/// What one connection to the fixture got.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Attempt {
    /// Answered, by the process with this pid, which keeps "the port
    /// answered" and "that process answered" separate claims.
    Served(u32),
    /// Refused, reset, timed out, or closed with nothing on it, carrying the
    /// reason. A connection accepted into a backlog whose listener then closed
    /// arrives as an empty answer, not a connect error.
    Failed(String),
}

impl Attempt {
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
                // Connections already open are waited out: those in flight at
                // the swap are the ones an instance killed mid-answer loses.
                let mut attempts = Vec::new();
                while let Some(outcome) = set.join_next().await {
                    attempts.push(outcome.expect("an attempt cannot panic"));
                }
                attempts
            }
        });
        Self { stop, task }
    }

    /// Stops connecting and reports every attempt, in-flight ones included.
    async fn finish(self) -> Vec<Attempt> {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.task.await.expect("the hammer cannot panic")
    }
}

/// A one-line tally of a run of attempts, for a failure message.
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
/// The bus cannot say when this sheep has bound: it configures neither a
/// channel nor a probe, so it is `Online` from the moment it is spawned.
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

/// Polls `ListFlock` until `id` is `Online`.
///
/// A probed app reaches `Online` a probe interval after it starts answering,
/// so [`await_serving`] returning is not the same fact, and a reload arriving
/// in that gap finds nothing replaceable.
#[cfg(unix)]
async fn await_online(client: &mut Client, id: u32) {
    let online = tokio::time::timeout(RECV_TIMEOUT, async {
        loop {
            let listed = client.request(Request::ListFlock).await;
            let Response::Flock(flock) = listed.result.unwrap() else {
                panic!("expected flock")
            };
            if flock
                .iter()
                .any(|info| info.id == id && info.status == ProcStatus::Online)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(online.is_ok(), "id {id} must reach Online");
}

/// A port with nothing on it: bind `:0`, read what the OS chose, release it.
///
/// Check-then-use: a stranger can take the port before the fixture binds it.
/// That loss is loud, since the fixture panics with the bind error.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("the OS must have a free loopback port")
        .local_addr()
        .expect("a bound listener has an address")
        .port()
}

/// The fixture server's binary, built as `examples/reuse_port_sheep.rs`.
///
/// Located rather than named: `env!("CARGO_BIN_EXE_<name>")` covers `[[bin]]`
/// targets only. Cargo puts an example at `<profile>/examples/<name>` and this
/// test binary at the sibling `deps/<name>-<hash>`.
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

#[cfg(unix)]
/// Reloads one `reuse_port_sheep` while a caller connects continuously, and
/// hands back every attempt made between the request and the swap finishing.
///
/// Asserts what holds whatever the app does with its stop signal: the swap
/// completes, the replacement answers the port, and the instance it replaced
/// is gone. The counting is the caller's.
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
    // Teardown's ladder, not the reload's: a defiant replacement is SIGKILLed
    // at the end of the test too, and the spec's 1.6s default would be spent
    // waiting for a process that never answers.
    app.kill_timeout = DRAIN_WINDOW;
    // Nothing may respawn behind the measurement: a restart would put a third
    // process on this port.
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

    // Answering as the process this test thinks is on the port rules out a
    // stranger or a leftover being the reason a later attempt fails.
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

    // `Reloaded` rather than a duration bounds the window to the reload.
    let replacement = client
        .await_any_process_event(ProcessEventKind::Reloaded)
        .await;
    let during = hammer.finish().await;
    let replacement_pid = replacement.pid.expect("a replacement has a pid");
    assert_ne!(replacement_pid, drainee_pid);
    assert_reaped(i32::try_from(drainee_pid).unwrap()).await;

    // One row, the replacement's: the drainee's registration went with the
    // process.
    let listed = client.request(Request::ListFlock).await;
    let Response::Flock(flock) = listed.result.unwrap() else {
        panic!("expected flock")
    };
    assert_eq!(flock.len(), 1);
    assert_eq!(flock[0].id, replacement.id);
    // The status catches a swap committed before its replacement could prove
    // anything, which costs no connections.
    assert_eq!(flock[0].status, ProcStatus::Online);

    // The fixture derives its port from `SHEP_INSTANCE`: a replacement in
    // another slot would answer somewhere else.
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

#[cfg(unix)]
/// shep promises the overlap, not zero downtime: a listener's accept backlog
/// is reset when it closes, so what is queued and unaccepted is lost unless
/// the app drains inside `graceful_timeout`.
///
/// The count is asserted on Linux only. Linux load-balances new connections
/// over every listener in the `SO_REUSEPORT` group, so the drainee keeps a
/// share until it closes; macOS gives every new connection to the last
/// socket to bind, so only the duration is asserted there.
#[tokio::test]
async fn a_reload_costs_a_draining_app_no_connections() {
    let during = reload_under_load("drainer", false).await;
    let failures = during.iter().filter(|attempt| attempt.failed()).count();
    // Printed on every platform, asserted on one.
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

#[cfg(unix)]
/// An instance that will not stop accepting, finish what it has, and exit
/// inside `graceful_timeout` reaches the end of that window still holding
/// work, and `SIGKILL` takes the work with it. No supervisor can give that
/// app zero downtime.
///
/// The count is asserted on Linux only, for the sibling case's reason: there
/// the defiant instance is still being handed a share of every new connection
/// when `SIGKILL` lands, while on macOS it is killed empty.
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

/// The reference smit, the one `shep-deploy` paints: a mark and a revision,
/// thirteen characters, and nothing shep understands.
const SMIT: &str = "\u{25b2} main@a1b2c3";

/// Starts one long-lived real sheep under `name` and answers with its id.
async fn start_sheep(client: &mut Client, name: &str) -> u32 {
    let app = forever_app(name);
    let started = client.request(Request::Start { apps: vec![app] }).await;
    let Response::Started(infos) = started.result.expect("the sheep must start") else {
        panic!("expected started")
    };
    infos[0].id
}

/// `name`'s smit as `shep flock` would paint it, read over the socket rather
/// than out of the daemon's memory.
async fn smit_of(client: &mut Client, name: &str) -> Option<String> {
    let listed = client.request(Request::ListFlock).await;
    let Response::Flock(flock) = listed.result.expect("the flock must list") else {
        panic!("expected flock")
    };
    flock
        .into_iter()
        .find(|info| info.name == name)
        .expect("the sheep must still be registered")
        .smit
}

/// Waits for `name`'s smit to clear, answering `false` at [`RECV_TIMEOUT`].
///
/// Polls: the daemon learns of a closed socket asynchronously.
async fn await_smit_cleared(client: &mut Client, name: &str) -> bool {
    tokio::time::timeout(RECV_TIMEOUT, async {
        while smit_of(client, name).await.is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

/// What has to hold is that closing a real socket reaches the forget path,
/// through `handle_conn`'s tail, the actor's mailbox and `to_info`.
#[tokio::test]
async fn a_smit_dies_with_the_connection_that_painted_it() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    // The observer must not be the connection whose closing is under test.
    let mut looker = fixture.connect().await;
    start_sheep(&mut looker, "web").await;

    let mut painter = fixture.connect().await;
    let painted = painter
        .request(Request::SetSmit {
            sheep: "web".to_string(),
            smit: Some(SMIT.parse().expect("the reference smit must be valid")),
        })
        .await;
    assert!(
        matches!(painted.result, Ok(Response::SmitPainted(_))),
        "{painted:?}"
    );

    assert_eq!(
        smit_of(&mut looker, "web").await,
        Some(SMIT.to_string()),
        "a smit must be visible to every client, not only its painter"
    );

    drop(painter);

    assert!(
        await_smit_cleared(&mut looker, "web").await,
        "the smit outlived the connection that painted it"
    );

    fixture.shutdown().await;
}

/// Also fails if a dog can clear a smit it did not paint: connection scoping
/// is otherwise indistinguishable from "any disconnect wipes everything".
#[tokio::test]
async fn one_dogs_disconnect_leaves_another_dogs_smit_alone() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut looker = fixture.connect().await;
    start_sheep(&mut looker, "web").await;
    start_sheep(&mut looker, "api").await;

    let mut deployer = fixture.connect().await;
    let mut watcher = fixture.connect().await;
    for (client, sheep) in [(&mut deployer, "web"), (&mut watcher, "api")] {
        let painted = client
            .request(Request::SetSmit {
                sheep: sheep.to_string(),
                smit: Some(SMIT.parse().expect("the reference smit must be valid")),
            })
            .await;
        assert!(
            matches!(painted.result, Ok(Response::SmitPainted(_))),
            "{painted:?}"
        );
    }

    // A clear only takes effect from the connection that painted it.
    let ignored = watcher
        .request(Request::SetSmit {
            sheep: "web".to_string(),
            smit: None,
        })
        .await;
    assert!(
        matches!(ignored.result, Ok(Response::SmitPainted(_))),
        "{ignored:?}"
    );
    assert_eq!(
        smit_of(&mut looker, "web").await,
        Some(SMIT.to_string()),
        "one dog cleared a smit another dog painted"
    );

    drop(deployer);

    assert!(
        await_smit_cleared(&mut looker, "web").await,
        "the smit outlived the connection that painted it"
    );
    assert_eq!(
        smit_of(&mut looker, "api").await,
        Some(SMIT.to_string()),
        "one dog's disconnect cleared another dog's smit"
    );

    fixture.shutdown().await;
}

/// The renderer is not the guard: `output::width::sanitize_cell` keeps a
/// well-formed CSI sequence, since shep's own colouring is made of them.
///
/// The frame is built past the `Smit` parser, so this is the daemon's refusal
/// rather than the client's. A malformed body ends the connection with no
/// reply, so either answer is a refusal and neither is a stored smit.
#[tokio::test]
async fn a_smit_carrying_an_escape_is_refused_at_the_daemon() {
    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut looker = fixture.connect().await;
    start_sheep(&mut looker, "web").await;

    let mut rogue = fixture.connect().await;
    let refused = rogue
        .request_raw(serde_json::json!({
            "kind": "set_smit",
            "sheep": "web",
            "smit": "\u{1b}[2Jgone",
        }))
        .await;
    assert!(
        refused.as_ref().is_none_or(|reply| reply.result.is_err()),
        "the daemon accepted a smit carrying an escape: {refused:?}"
    );
    assert_eq!(smit_of(&mut looker, "web").await, None);

    fixture.shutdown().await;
}

// --- Reload: a probed app's replacement answers for itself ---

/// The `AwaitReady` window a probed reload gets, as `listen_timeout`.
///
/// Both directions matter: a replacement that will serve has to bind inside
/// it, and one that will not costs the case its whole length before the
/// abandonment lands.
///
/// `cfg(unix)`: `reuse_port_sheep` has no Windows build under that name.
#[cfg(unix)]
const PROBED_READY_WINDOW: UpDuration = UpDuration::from_millis(800);

/// One `reuse_port_sheep` on `port`, gated on a TCP probe against the port it
/// binds: the arrangement in which "is the new instance ready" and "is
/// something listening" are the same question.
#[cfg(unix)]
fn probed_sheep(name: &str, port: u16, mute_file: &std::path::Path) -> AppConfig {
    let mut app = AppConfig::minimal(name, &reuse_port_sheep().display().to_string());
    app.interpreter = Some("none".to_string());
    app.env
        .insert("SHEEP_PORT_BASE".to_string(), port.to_string());
    app.env.insert("SHEEP_HOLD_MS".to_string(), "0".to_string());
    app.env.insert(
        "SHEEP_MUTE_FILE".to_string(),
        mute_file.display().to_string(),
    );
    app.readiness_probe = Some(ProbeConfig {
        kind: ProbeKind::Tcp,
        target: format!("127.0.0.1:{port}"),
        interval: UpDuration::from_millis(50),
        timeout: UpDuration::from_millis(200),
        failure_threshold: 3,
    });
    app.listen_timeout = PROBED_READY_WINDOW;
    app.graceful_timeout = DRAIN_WINDOW;
    app.kill_timeout = DRAIN_WINDOW;
    // Nothing may respawn behind the case: a restart would put a third process
    // on this port.
    app.autorestart = false;
    app
}

/// The control for the case below: without it, an implementation that
/// abandoned every probed reload would pass the failure case and look correct.
///
/// The replacement must land in the drainee's instance slot, since the fixture
/// derives its port from `SHEP_INSTANCE`.
#[cfg(unix)]
#[tokio::test]
async fn a_probed_reload_of_a_working_release_still_finishes() {
    let _port_guard = RELOAD_PORT_LOCK.lock().await;
    let port = free_port();
    let dir = tempfile::tempdir().unwrap();
    let mute = dir.path().join("mute");

    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;

    let started = client
        .request(Request::Start {
            apps: vec![probed_sheep("web", port, &mute)],
        })
        .await;
    let Response::Started(infos) = started.result.unwrap() else {
        panic!("expected started")
    };
    let drainee_id = infos[0].id;
    let drainee_pid = infos[0].pid.expect("a real spawn reports a real pid");
    await_serving(port, drainee_pid).await;
    await_online(&mut client, drainee_id).await;

    // Subscribed after the app is up: this case reads the event stream in
    // emission order, and an earlier subscription would put `Start` in front.
    let subscribed = client
        .request(Request::Subscribe {
            topics: vec!["process.*".to_string()],
        })
        .await;
    assert_eq!(subscribed.result.unwrap(), Response::Subscribed);

    let accepted = client
        .request(Request::Reload {
            selector: SelectorSpec::Name("web".to_string()),
        })
        .await;
    let Response::Reloading(accepted) = accepted.result.unwrap() else {
        panic!("expected an accepted reload")
    };
    assert_eq!(accepted.len(), 1);

    // In order: the question is which of the two endings the reload reached,
    // and a search would find one whatever else had already been said.
    let (ending, replacement) = client
        .next_process_event_of(&[
            ProcessEventKind::Reloaded,
            ProcessEventKind::ReloadAbandoned,
        ])
        .await;
    assert_eq!(
        ending,
        ProcessEventKind::Reloaded,
        "a serial reload is slower than an overlapping one, not broken"
    );
    let replacement_pid = replacement.pid.expect("a replacement has a pid");
    assert_ne!(replacement_pid, drainee_pid);
    assert_eq!(replacement.status, ProcStatus::Online);
    assert_eq!(
        tally(&burst(port).await),
        format!("{BURST}x served by {replacement_pid}"),
        "the replacement owns the port once the swap is done"
    );

    let held = fixture.shutdown().await;
    drop(held);
    drop(dir);
}

/// Both instances run the same command; the second finds `SHEEP_MUTE_FILE` in
/// place and binds nothing, which is what a release whose listener moved to
/// the wrong port does. Its first probe is otherwise answered by the
/// instance still bound to that address.
///
/// Two independent mechanisms cover a single-instance probed app and each is
/// enough alone: the serial reload mode, and the post-drain probe. The flock
/// row's status is what a deploy tool reads, so it is asserted too.
#[cfg(unix)]
#[tokio::test]
async fn a_replacement_that_serves_nothing_is_refused_not_reported_reloaded() {
    let _port_guard = RELOAD_PORT_LOCK.lock().await;
    let port = free_port();
    let dir = tempfile::tempdir().unwrap();
    let mute = dir.path().join("mute");

    let fixture = Fixture::boot(tempfile::tempdir().unwrap(), false).await;
    let mut client = fixture.connect().await;

    let started = client
        .request(Request::Start {
            apps: vec![probed_sheep("web", port, &mute)],
        })
        .await;
    let Response::Started(infos) = started.result.unwrap() else {
        panic!("expected started")
    };
    let drainee_id = infos[0].id;
    let drainee_pid = infos[0].pid.expect("a real spawn reports a real pid");
    await_serving(port, drainee_pid).await;
    await_online(&mut client, drainee_id).await;

    // Subscribed after the app is up: this case reads the event stream in
    // emission order, and an earlier subscription would put `Start` in front.
    let subscribed = client
        .request(Request::Subscribe {
            topics: vec!["process.*".to_string()],
        })
        .await;
    assert_eq!(subscribed.result.unwrap(), Response::Subscribed);

    // The bad release, staged between the two spawns of one unchanged app.
    std::fs::write(&mute, b"").expect("the marker must be writable");

    let accepted = client
        .request(Request::Reload {
            selector: SelectorSpec::Name("web".to_string()),
        })
        .await;
    let Response::Reloading(accepted) = accepted.result.unwrap() else {
        panic!("expected an accepted reload")
    };
    assert_eq!(accepted.len(), 1);

    // In order again, for the reason the control case gives.
    let (ending, info) = client
        .next_process_event_of(&[
            ProcessEventKind::Reloaded,
            ProcessEventKind::ReloadAbandoned,
        ])
        .await;
    assert_eq!(
        ending,
        ProcessEventKind::ReloadAbandoned,
        "a replacement that binds nothing has not proved it can take over"
    );
    assert_ne!(info.id, drainee_id, "the abandonment names the replacement");

    let listed = client.request(Request::ListFlock).await;
    let Response::Flock(flock) = listed.result.unwrap() else {
        panic!("expected flock")
    };
    assert_eq!(flock.len(), 1, "the app keeps the one instance it has left");
    assert_eq!(
        flock[0].status,
        ProcStatus::Starting,
        "a replacement that never answered its probe is never called online"
    );

    let held = fixture.shutdown().await;
    drop(held);
    drop(dir);
}
