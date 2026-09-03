//! Drives the `answers` example's handlers as a real child, on a real
//! channel, on whichever platform this is.
//!
//! Both halves of the contract are here, because they are two different
//! mechanisms and only the assertions are shared: unix wires an inherited
//! descriptor onto the child's fd 3, and Windows hands it the name of a
//! pipe it opens itself. [`ShepherdSide`] is defined twice, once per
//! platform, and the test body below calls the same two methods on whichever
//! one compiled.
//!
//! # What the Windows half is for
//!
//! It ran nothing until 2026-09-02. CI's Windows legs type-checked
//! `endpoint.rs`'s `Endpoint::Pipe` arm and executed no line of it, and the
//! arm was broken: the shepherd hands an app **one** pipe instance, so
//! `try_clone` gives the writer thread a second handle onto the same
//! synchronous kernel file object, and the reader thread parked in
//! `ReadFile` held that object against every write the app tried to make.
//! `ready()` could not get out, so the shepherd never heard it and never
//! sent anything, so the read never completed. Measured here, on real
//! Windows: the write returned only when the pipe was torn down.
//!
//! That is why the first assertion below is worth reading twice. On Windows
//! it is not checking that the app said the right thing -- it is checking
//! that the app could say anything at all.
//!
//! The shepherd end is the production one rather than a lookalike. It is the
//! same `ServerOptions` call `shep_core::transport::Listener::bind` makes,
//! down to `first_pipe_instance` and `reject_remote_clients`, so a bug in
//! how the daemon creates the pipe would show up here too.
//!
//! # Why this does not spawn `examples/answers.rs`
//!
//! Cargo defines `CARGO_BIN_EXE_<name>` for a `[[bin]]` target, not for an
//! example, so spawning the example by that variable is a compile error,
//! not a runtime surprise -- measured 2026-09-02. `cargo test --test
//! real_child` does not build examples at all, and the usual
//! `current_exe()/../examples/<name>` path trick does not hold on this
//! machine either: cargo puts test binaries under `build/<pkg>/<hash>/out/`,
//! a layout this repo's own `rust-toolchain.toml` already records breaking
//! two tests before.
//!
//! Instead, this file's one test re-execs *itself*. [`CHILD_VAR`], checked
//! at the very top of the test function, tells a re-exec of the same test
//! binary to run [`run_as_child`] -- the same handlers `examples/answers.rs`
//! registers -- instead of running the assertions below. The child is still
//! a real separate process finding a real channel, which is the property
//! under test; `examples/answers.rs` stays as the copy an app author reads,
//! and `clippy --all-targets` keeps it from rotting.

use std::process::{Command, Stdio};
use std::time::Duration;

/// Set in the child's environment to mean "you are the child: run the
/// handlers instead of the assertions." Checked before anything else in the
/// test function, so nothing above it can accidentally run twice.
const CHILD_VAR: &str = "SHEP_CHANNEL_TEST_CHILD";

/// This file's one test function's name, passed to the re-exec'd binary as
/// an exact filter. Keep this in sync with the `#[test] fn` below by hand
/// -- libtest names tests by plain string, so nothing else enforces it.
const TEST_NAME: &str = "a_real_child_finds_its_channel_and_answers";

/// How long the parent gives the child to answer before calling it hung. A
/// working child answers in milliseconds; this is slack for a loaded
/// runner, not an expected duration -- the same convention `outbox.rs` and
/// `session.rs` document theirs with. Every read below is bounded by it, so
/// a child that never answers fails this test instead of hanging the suite.
const DEADLINE: Duration = Duration::from_secs(10);

/// A substring of `serve.rs`'s private no-channel advice. Not the whole
/// string -- that constant is not exported from the crate -- just enough to
/// prove the warning did not fire.
const NO_CHANNEL_SNIPPET: &str = "no channel on this process";

/// The child half: registers the same handlers `examples/answers.rs` does,
/// says ready, emits one metric, and parks. Never returns -- the parent
/// kills this process once its assertions are done, so there is nothing to
/// fall through to.
fn run_as_child() -> ! {
    let shepherd = shep_channel::serve();
    shepherd.on_action("gc", |params, _name| {
        format!("collected, params={params:?}")
    });
    shepherd.on_shutdown(|| std::process::exit(0));
    shepherd.ready().expect("say ready");
    shepherd.metric("rps", 42.0);

    // Park. The reader thread is doing the work; the parent kills this
    // process once it is done asserting.
    loop {
        std::thread::park();
    }
}

#[test]
fn a_real_child_finds_its_channel_and_answers() {
    if std::env::var_os(CHILD_VAR).is_some() {
        run_as_child();
    }

    let (mut shepherd, child) = open_channel_and_spawn_child();
    let child = ChildGuard(Some(child));

    // On Windows this is the deadlock check, not a formatting check: an app
    // whose writer thread is stuck behind its own parked reader never gets
    // this far, because the shepherd is waiting for exactly this line before
    // it sends anything.
    assert_eq!(
        shepherd.read_line(),
        "{\"kind\":\"ready\"}",
        "the child never said ready"
    );

    assert_eq!(
        shepherd.read_line(),
        "{\"kind\":\"metric\",\"name\":\"rps\",\"value\":42.0}"
    );

    shepherd.write_line("{\"kind\":\"action\",\"name\":\"gc\",\"params\":\"now\",\"id\":7}");
    assert_eq!(
        shepherd.read_line(),
        "{\"kind\":\"action-reply\",\"action\":\"gc\",\"body\":\"collected, params=Some(\\\"now\\\")\",\"id\":7}"
    );

    // The rule this crate exists for, against a real process.
    shepherd.write_line("{\"kind\":\"action\",\"name\":\"typo\",\"id\":8}");
    assert_eq!(
        shepherd.read_line(),
        "{\"kind\":\"action-reply\",\"action\":\"typo\",\"body\":\"unknown action: typo\",\"id\":8}"
    );

    let mut child = child.take();
    child.kill().expect("kill");
    // `wait_with_output`, not a bare `wait`, because it also collects the
    // stderr piped below without a manual read -- the process is already
    // dead, so this returns as soon as the OS reaps it.
    let output = child.wait_with_output().expect("reap and collect stderr");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains(NO_CHANNEL_SNIPPET),
        "a process with a real channel warned as though it had none: {stderr}"
    );
}

/// The parts of the child's command that do not depend on how the channel
/// reaches it.
///
/// `SHEP_NAME` is load-bearing rather than decoration: `serve()` gates its
/// no-channel advice on that variable, so without it the child would stay
/// silent for the wrong reason and the stderr assertion above would prove
/// nothing. With it set, the child is a process running under shep that
/// DOES have a channel, so the advice must not fire.
fn base_command() -> Command {
    let exe = std::env::current_exe().expect("path to this test binary");
    let mut command = Command::new(exe);
    command
        .args(["--exact", TEST_NAME, "--nocapture"])
        .env(CHILD_VAR, "1")
        .env("SHEP_CHANNEL_VERSION", "1")
        .env("SHEP_NAME", "answers")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Kills and reaps the wrapped child on drop if [`ChildGuard::take`] was
/// never called -- including on an unwind from a failed `assert_eq!` above.
/// Every read in this test is bounded by [`DEADLINE`], so a hung child
/// fails an assertion rather than blocking forever, but a failed assertion
/// alone does not kill the child that caused it; without this guard that
/// child parks as an orphan (reparented to pid 1) for as long as the
/// machine runs. Verified: deliberately breaking `endpoint::discover` to
/// return `Absent` (task-7's non-vacuity check) panics the parent at the
/// first read and, without this guard, leaves exactly that orphan behind.
struct ChildGuard(Option<std::process::Child>);

impl ChildGuard {
    /// Hands back the child for an explicit kill, disarming the drop guard.
    ///
    /// # Panics
    ///
    /// If called more than once; this test calls it exactly once, on the
    /// success path after every assertion above has already passed.
    #[track_caller]
    fn take(mut self) -> std::process::Child {
        self.0.take().expect("ChildGuard::take called twice")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The shepherd's end of a socketpair, read and written a line at a time.
#[cfg(unix)]
struct ShepherdSide {
    reader: std::io::BufReader<std::os::unix::net::UnixStream>,
    writer: std::os::unix::net::UnixStream,
}

#[cfg(unix)]
impl ShepherdSide {
    /// The next line the child sent, without its newline.
    ///
    /// # Panics
    ///
    /// If the child sends nothing within [`DEADLINE`], or closes its end.
    #[track_caller]
    fn read_line(&mut self) -> String {
        use std::io::BufRead as _;

        let mut line = String::new();
        let read = self
            .reader
            .read_line(&mut line)
            .expect("the child sent nothing within the deadline");
        assert!(read > 0, "the child closed its end of the channel");
        line.trim_end().to_string()
    }

    /// Sends one line to the child, newline appended.
    ///
    /// # Panics
    ///
    /// If the write fails.
    #[track_caller]
    fn write_line(&mut self, line: &str) {
        use std::io::Write as _;

        self.writer
            .write_all(format!("{line}\n").as_bytes())
            .expect("write to the socket");
    }
}

/// Spawns the child with a socketpair mapped onto its fd 3, and keeps the
/// shepherd's end.
///
/// `FdMapping` takes an `OwnedFd`, which a `UnixStream` converts into, so
/// this needs no `unsafe` at all.
#[cfg(unix)]
fn open_channel_and_spawn_child() -> (ShepherdSide, std::process::Child) {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

    use command_fds::{CommandFdExt as _, FdMapping};

    let (ours, theirs) = UnixStream::pair().expect("socketpair");
    // The child inherits a blocking descriptor, which is what the shepherd
    // hands a real app: `tokio_runner.rs` clears O_NONBLOCK deliberately so
    // a plain read parks rather than returning EAGAIN.
    theirs.set_nonblocking(false).expect("blocking");
    ours.set_read_timeout(Some(DEADLINE)).expect("deadline");

    let mut command = base_command();
    command
        .env_remove("SHEP_CHANNEL_PIPE")
        .env("SHEP_CHANNEL_FD", "3");
    command
        .fd_mappings(vec![FdMapping {
            parent_fd: OwnedFd::from(theirs),
            child_fd: 3,
        }])
        .expect("map the socketpair to fd 3");
    let child = command
        .spawn()
        .expect("spawn the re-exec'd test binary as the channel child");

    let writer = ours.try_clone().expect("clone");
    let side = ShepherdSide {
        reader: std::io::BufReader::new(ours),
        writer,
    };
    (side, child)
}

/// The shepherd's end of a named pipe, read and written a line at a time.
///
/// Async underneath because the server end of a Windows named pipe is: the
/// daemon's own is `tokio::net::windows::named_pipe::NamedPipeServer`, and
/// using anything else here would test a pipe the daemon does not create.
/// Every call blocks on the runtime, so the test body reads the same on both
/// platforms.
#[cfg(windows)]
struct ShepherdSide {
    runtime: tokio::runtime::Runtime,
    pipe: tokio::io::BufReader<tokio::net::windows::named_pipe::NamedPipeServer>,
}

#[cfg(windows)]
impl ShepherdSide {
    /// The next line the child sent, without its newline.
    ///
    /// # Panics
    ///
    /// If the child sends nothing within [`DEADLINE`], or closes its end.
    #[track_caller]
    fn read_line(&mut self) -> String {
        use tokio::io::AsyncBufReadExt as _;

        // Split the borrow: `block_on` takes `&self` on the runtime while
        // the read needs `&mut` on the pipe, and they are different fields.
        let Self { runtime, pipe } = self;
        let mut line = String::new();
        let read = runtime
            .block_on(async { tokio::time::timeout(DEADLINE, pipe.read_line(&mut line)).await })
            .expect("the child sent nothing within the deadline")
            .expect("read from the pipe");
        assert!(read > 0, "the child closed its end of the channel");
        line.trim_end().to_string()
    }

    /// Sends one line to the child, newline appended.
    ///
    /// # Panics
    ///
    /// If the write fails.
    #[track_caller]
    fn write_line(&mut self, line: &str) {
        use tokio::io::AsyncWriteExt as _;

        let Self { runtime, pipe } = self;
        let framed = format!("{line}\n");
        runtime
            .block_on(async {
                pipe.write_all(framed.as_bytes()).await?;
                pipe.flush().await
            })
            .expect("write to the pipe");
    }
}

/// Creates the channel's pipe, spawns the child pointed at it by name, and
/// waits for the child to open it.
///
/// The order matters and is the daemon's: the pipe exists before the child
/// does, so the child's own open cannot lose a race with it. `connect`
/// resolves once the child has opened its end -- which `serve()` does at
/// startup, before it registers a single handler.
#[cfg(windows)]
fn open_channel_and_spawn_child() -> (ShepherdSide, std::process::Child) {
    use tokio::net::windows::named_pipe::ServerOptions;

    // Unique per process, as the daemon's is unique per spawn. This test is
    // the only thing in its own process that opens one, so a pid is enough
    // where the daemon needs a nonce.
    let name = format!(r"\\.\pipe\shep-channel-test-{}", std::process::id());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime for the shepherd end");
    // `ServerOptions::create` registers with the reactor, so it has to run
    // inside the runtime's context even though it is not itself async.
    let server = {
        let _guard = runtime.enter();
        ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create(&name)
            .expect("create the channel's pipe")
    };

    let mut command = base_command();
    command
        .env_remove("SHEP_CHANNEL_FD")
        .env("SHEP_CHANNEL_PIPE", &name);
    let child = command
        .spawn()
        .expect("spawn the re-exec'd test binary as the channel child");

    runtime
        .block_on(async { tokio::time::timeout(DEADLINE, server.connect()).await })
        .expect("the child never opened the pipe within the deadline")
        .expect("accept the child's connection");

    let side = ShepherdSide {
        pipe: tokio::io::BufReader::new(server),
        runtime,
    };
    (side, child)
}
