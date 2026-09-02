//! Drives the `answers` example's handlers as a real child with a real
//! fd 3.
//!
//! unix only: wiring an inherited descriptor is the unix half of the
//! contract. The Windows half is a named pipe the app opens by name, which
//! needs a live shepherd to create, so it is covered by the shep daemon's
//! own Windows tests rather than here. Everything above the descriptor is
//! already covered on both platforms by the generic tests in `session.rs`.
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
//! registers -- instead of running the assertions below. The parent spawns
//! `current_exe()` filtered to this one test's name via `--exact`, with the
//! socketpair mapped onto its fd 3. The child is still a real separate
//! process finding a real inherited descriptor, which is the property under
//! test; `examples/answers.rs` stays as the copy an app author reads, and
//! `clippy --all-targets` keeps it from rotting.
#![cfg(unix)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Set in the child's environment to mean "you are the child: run the
/// handlers instead of the assertions." Checked before anything else in the
/// test function, so nothing above it can accidentally run twice.
const CHILD_VAR: &str = "SHEP_CHANNEL_TEST_CHILD";

/// This file's one test function's name, passed to the re-exec'd binary as
/// an exact filter. Keep this in sync with the `#[test] fn` below by hand
/// -- libtest names tests by plain string, so nothing else enforces it.
const TEST_NAME: &str = "a_real_child_finds_fd_3_and_answers";

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
fn a_real_child_finds_fd_3_and_answers() {
    if std::env::var_os(CHILD_VAR).is_some() {
        run_as_child();
    }

    let (ours, theirs) = UnixStream::pair().expect("socketpair");
    // The child inherits a blocking descriptor, which is what the shepherd
    // hands a real app: `tokio_runner.rs` clears O_NONBLOCK deliberately so
    // a plain read parks rather than returning EAGAIN.
    theirs.set_nonblocking(false).expect("blocking");
    ours.set_read_timeout(Some(DEADLINE)).expect("deadline");

    let child = ChildGuard(Some(spawn_child(theirs)));

    let mut writer = ours.try_clone().expect("clone");
    let mut reader = BufReader::new(ours);

    let mut ready = String::new();
    reader
        .read_line(&mut ready)
        .expect("child did not say ready within the deadline");
    assert_eq!(ready.trim_end(), "{\"kind\":\"ready\"}");

    let mut metric = String::new();
    reader.read_line(&mut metric).expect("no metric");
    assert_eq!(
        metric.trim_end(),
        "{\"kind\":\"metric\",\"name\":\"rps\",\"value\":42.0}"
    );

    writer
        .write_all(b"{\"kind\":\"action\",\"name\":\"gc\",\"params\":\"now\",\"id\":7}\n")
        .expect("write");
    let mut reply = String::new();
    reader.read_line(&mut reply).expect("no reply");
    assert_eq!(
        reply.trim_end(),
        "{\"kind\":\"action-reply\",\"action\":\"gc\",\"body\":\"collected, params=Some(\\\"now\\\")\",\"id\":7}"
    );

    // The rule this crate exists for, against a real process.
    writer
        .write_all(b"{\"kind\":\"action\",\"name\":\"typo\",\"id\":8}\n")
        .expect("write");
    let mut unknown = String::new();
    reader
        .read_line(&mut unknown)
        .expect("no reply to an unknown action");
    assert_eq!(
        unknown.trim_end(),
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

/// Kills and reaps the wrapped child on drop if [`ChildGuard::take`] was
/// never called -- including on an unwind from a failed `assert_eq!` above.
/// Every read in this test is bounded by [`DEADLINE`], so a hung child
/// fails an assertion rather than blocking forever, but a failed assertion
/// alone does not kill the child that caused it; without this guard that
/// child parks as an orphan (reparented to pid 1) for as long as the
/// machine runs. Verified: deliberately breaking `endpoint::discover` to
/// return `Absent` (task-7's non-vacuity check) panics the parent at the
/// first `read_line` and, without this guard, leaves exactly that orphan
/// behind.
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

/// Spawns a re-exec of this same test binary, filtered by `--exact` to run
/// only [`run_as_child`] (by way of [`CHILD_VAR`]), with `theirs` mapped
/// onto its fd 3. `FdMapping` takes an `OwnedFd`, which a `UnixStream`
/// converts into, so this needs no `unsafe` at all.
fn spawn_child(theirs: UnixStream) -> std::process::Child {
    use std::os::fd::OwnedFd;

    use command_fds::{CommandFdExt as _, FdMapping};

    let exe = std::env::current_exe().expect("path to this test binary");
    let mut command = Command::new(exe);
    command
        .args(["--exact", TEST_NAME, "--nocapture"])
        .env(CHILD_VAR, "1")
        .env("SHEP_CHANNEL_FD", "3")
        .env("SHEP_CHANNEL_VERSION", "1")
        // Set so the child takes the no-channel warning path only if it has
        // no channel, which it does. Without this the test would prove
        // nothing about a process running under shep.
        .env("SHEP_NAME", "answers")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
        .fd_mappings(vec![FdMapping {
            parent_fd: OwnedFd::from(theirs),
            child_fd: 3,
        }])
        .expect("map the socketpair to fd 3");
    command
        .spawn()
        .expect("spawn the re-exec'd test binary as the channel child")
}
