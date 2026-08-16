//! The PID-1 init split, driven as a real process: `commands::reap`'s own
//! reason to exist — signal forwarding and the relayed exit status — has no
//! coverage anywhere else in the suite. `cargo test -p shep-cli --lib` only
//! proves `classify` and `should_split` behave in isolation; neither test
//! there can fail if the whole forwarding arm were deleted from `run_init`'s
//! `select!`, which is exactly the regression this file exists to catch.
//!
//! `#![cfg(unix)]` for the same reason `cli_e2e.rs` carries it: an
//! integration test file is its own compilation unit, so without the guard
//! `--all-targets` would build it (uselessly, since `commands` itself is
//! `#[cfg(unix)]`) on the Windows CI leg.
//!
//! Every case here sets `SHEP_FORCE_INIT=1` — a test harness is never PID 1,
//! so without it `should_split` never fires and there would be nothing to
//! signal. See `commands::reap::should_split`'s own doc.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use assert_cmd::cargo::CommandCargoExt as _;
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use tempfile::TempDir;

/// Bound on every wait in this file: reading the init's own "supervising
/// pid" line, polling for the flock to come online, and the process's own
/// exit after a signal. A few seconds, like `CRON_DEADLINE` in
/// `cli_e2e.rs` — the whole claim under test is that the init answers a
/// signal promptly rather than riding a grace period, so this is generous
/// headroom for a loaded CI machine's boot-and-start round trip, not a
/// second budget being smuggled in.
const INIT_DEADLINE: Duration = Duration::from_secs(10);

/// Writes an executable script into `dir` and returns its path. The
/// executable bit is the point: without it every `shep runtime` fails
/// EACCES for a reason that has nothing to do with the init.
fn write_script(dir: &TempDir, name: &str, contents: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.path().join(name);
    std::fs::write(&path, contents).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

/// Writes `Flockfile.toml` into `dir` and returns its path.
fn write_flockfile(dir: &TempDir, body: &str) -> PathBuf {
    let path = dir.path().join("Flockfile.toml");
    std::fs::write(&path, body).unwrap();
    path
}

/// A long-lived, single-app Flockfile: a bare `sleep 60`, which dies on the
/// very first `SIGTERM` (its default disposition) and needs no readiness
/// handshake — the point is a sheep still running when the signal arrives,
/// not the shape of its script.
fn write_held_flockfile(dir: &TempDir) -> PathBuf {
    let script = write_script(dir, "held.sh", "#!/bin/sh\nsleep 60\n");
    write_flockfile(
        dir,
        &format!(
            "[[app]]\nname = \"held\"\nscript = \"{}\"\n",
            script.display()
        ),
    )
}

/// Spawns `shep runtime <flockfile>` with `$SHEP_FORCE_INIT=1`, stdout and
/// stderr both piped so the caller can read the init's own "supervising
/// pid" line off stderr without either pipe filling up and blocking the
/// child.
fn spawn_forced_runtime(home: &Path, flockfile: &Path) -> Child {
    Command::cargo_bin("shep")
        .expect("locate the built shep binary")
        .arg("--home")
        .arg(home)
        .arg("runtime")
        .arg(flockfile)
        .env("SHEP_FORCE_INIT", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn shep runtime")
}

/// Reads `source` line by line on a background thread, forwarding each
/// line onto the returned channel. Backgrounding this — rather than
/// reading inline — is what lets the caller apply its own bound
/// ([`recv_pid_line`]) instead of blocking on the pipe forever.
fn spawn_line_reader<R: Read + Send + 'static>(source: R) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(source).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// Copies `source` to nowhere, on a background thread. `shep runtime`
/// streams the flock's bleats to its own stdout for as long as it runs; if
/// nothing drains that pipe it eventually fills and blocks the child,
/// wedging the whole test. This is that drain for the one stream this file
/// never needs to read.
fn discard_in_background<R: Read + Send + 'static>(mut source: R) {
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut source, &mut std::io::sink());
    });
}

/// Blocks on `rx` until a line matching `shep runtime: init supervising
/// pid <N>` arrives, and returns `N`. Panics past `deadline` — this is the
/// stderr line `commands::reap::run_init` prints right after spawning the
/// supervisor, so its absence within a generous bound means the process
/// never got that far.
fn recv_pid_line(rx: &Receiver<String>, deadline: Duration) -> i32 {
    const PREFIX: &str = "shep runtime: init supervising pid ";
    let start = Instant::now();
    loop {
        let remaining = deadline.saturating_sub(start.elapsed());
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                if let Some(pid) = line.strip_prefix(PREFIX) {
                    return pid.trim().parse().unwrap_or_else(|e| {
                        panic!("pid line carried no valid pid ({e}): {line:?}")
                    });
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                panic!("init did not print `{PREFIX}<pid>` within {deadline:?}")
            }
            Err(RecvTimeoutError::Disconnected) => {
                panic!("init's stderr closed before printing `{PREFIX}<pid>`")
            }
        }
    }
}

/// Polls `shep --home <home> flock --format json` until `done` accepts the
/// one sheep's row, or `deadline` elapses. A fresh `shep` invocation reaches
/// the same daemon `shep runtime` booted in-process, over the socket it
/// bound at `home`.
fn poll_online_sheep(home: &Path, deadline: Duration) -> serde_json::Value {
    let start = Instant::now();
    loop {
        let output = Command::cargo_bin("shep")
            .expect("locate the built shep binary")
            .arg("--home")
            .arg(home)
            .arg("--format")
            .arg("json")
            .arg("flock")
            .output()
            .expect("run shep flock");
        if output.status.success()
            && let Ok(envelope) = serde_json::from_slice::<serde_json::Value>(&output.stdout)
            && envelope["data"][0]["status"] == "online"
        {
            return envelope["data"][0].clone();
        }
        if start.elapsed() >= deadline {
            panic!(
                "the flock never reached `online` within {deadline:?}; last output: {}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Polls `child.try_wait()` until it exits, or `timeout` elapses — a named
/// panic instead of relying on the harness's own process timeout, which
/// would fail the whole binary and name nothing (IR-46's distinction,
/// applied to a plain thread wait rather than an `await`).
fn wait_bounded(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("poll shep runtime") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("shep runtime did not exit within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Best-effort cleanup so a panicking assertion never leaves a real daemon
/// and a real sleeping sheep behind: SIGKILLs the supervisor (if its pid
/// was ever learned) and then the init itself. On every success path in
/// this file both are already gone by the time this runs, so `kill`/`wait`
/// here are no-ops whose errors are ignored.
struct RuntimeGuard {
    child: Child,
    supervisor_pid: Option<i32>,
}

impl Drop for RuntimeGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.supervisor_pid {
            let _ = signal::kill(Pid::from_raw(pid), Signal::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// fails if the init ignores `SIGTERM`, or exits with its own status
/// instead of the supervisor's. `commands::reap::run_init`'s whole reason
/// to exist is the four forwarded signals — deleting that arm reddens
/// this and nothing else in the phase (Step 10.7, mutation 3).
///
/// Three things are asserted in order: the deadline bounds a clean stop
/// (an init that does not forward exits when the harness gives up, not
/// when told to), the exit status is the supervisor's own 0, and the held
/// sheep did not outlive the process that was supposed to stop it.
#[test]
fn a_sigterm_to_the_init_reaches_the_flock_and_the_status_is_the_childs() {
    let dir = tempfile::tempdir().unwrap();
    let flockfile = write_held_flockfile(&dir);

    let mut child = spawn_forced_runtime(dir.path(), &flockfile);
    let init_pid = child.id() as i32;
    let stderr_rx = spawn_line_reader(child.stderr.take().unwrap());
    discard_in_background(child.stdout.take().unwrap());

    let mut guard = RuntimeGuard {
        child,
        supervisor_pid: None,
    };
    let supervisor_pid = recv_pid_line(&stderr_rx, INIT_DEADLINE);
    guard.supervisor_pid = Some(supervisor_pid);

    let online = poll_online_sheep(dir.path(), INIT_DEADLINE);
    let sheep_pid = online["pid"]
        .as_i64()
        .unwrap_or_else(|| panic!("a real pid: {online}")) as i32;

    signal::kill(Pid::from_raw(init_pid), Signal::SIGTERM).expect("send SIGTERM to the init");

    let status = wait_bounded(&mut guard.child, INIT_DEADLINE);
    assert_eq!(
        status.code(),
        Some(0),
        "a clean stop is the supervisor's own 0"
    );
    assert!(
        signal::kill(Pid::from_raw(sheep_pid), None).is_err(),
        "the held sheep (pid {sheep_pid}) must not outlive the container"
    );
}

/// fails if a signalled supervisor does not make the init exit `128 +
/// signal`. This is the one assertion in the phase that reads the
/// PROCESS's status rather than `classify`'s return value, and it is what
/// proves `run_init` steps around the `ExitCode` funnel rather than being
/// clamped by it — `ExitCode` has no variant for 137 and never will (Step
/// 10.7, mutation 4: hardcoding the exit status reddens this while every
/// `classify` unit test still passes).
#[test]
fn a_supervisor_killed_by_sigkill_makes_the_init_exit_137() {
    let dir = tempfile::tempdir().unwrap();
    let flockfile = write_held_flockfile(&dir);

    let mut child = spawn_forced_runtime(dir.path(), &flockfile);
    let stderr_rx = spawn_line_reader(child.stderr.take().unwrap());
    discard_in_background(child.stdout.take().unwrap());

    let mut guard = RuntimeGuard {
        child,
        supervisor_pid: None,
    };
    let supervisor_pid = recv_pid_line(&stderr_rx, INIT_DEADLINE);
    guard.supervisor_pid = Some(supervisor_pid);

    signal::kill(Pid::from_raw(supervisor_pid), Signal::SIGKILL)
        .expect("SIGKILL the supervisor directly");

    let status = wait_bounded(&mut guard.child, INIT_DEADLINE);
    assert_eq!(
        status.code(),
        Some(137),
        "128 + SIGKILL, what `docker inspect` reads"
    );
    guard.supervisor_pid = None; // already dead; nothing left for Drop to kill
}

/// fails if the drain loop leaves a reparented orphan as a zombie — the
/// container's actual failure mode, a process table that fills up over
/// days. Linux only: `PR_SET_CHILD_SUBREAPER` makes this test process the
/// reaper for its own descendants, which is the only way to observe
/// reparenting without being PID 1 or holding a PID namespace; macOS has
/// no equivalent.
///
/// This does not go through the `shep` binary — `lib.rs`'s whole public
/// surface is three functions (decision 1), so nothing in `commands::reap`
/// is reachable from an external test crate. What it proves instead is the
/// mechanism `commands::reap::drain` relies on: that a bounded
/// `waitpid(-1, WNOHANG)` loop, run by a subreaper, actually reaps a
/// grandchild the kernel reparented to it, rather than leaving it a
/// zombie forever.
///
/// This machine is macOS, so this test did not run here; it is CI's Linux
/// runner's to execute.
#[cfg(target_os = "linux")]
#[test]
fn a_reparented_orphan_is_reaped() {
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};

    nix::sys::prctl::set_child_subreaper(true).expect("Linux has supported this since 3.4");

    // A shell that forks a short-lived grandchild and exits immediately,
    // so the grandchild is reparented to this test process.
    let mut shell = std::process::Command::new("/bin/sh")
        .args(["-c", "(sleep 0.2; exit 3) & exit 0"])
        .spawn()
        .expect("spawn the shell");
    let _ = shell.wait();

    // Bounded drain: never `loop {}` against a real kernel event.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut reaped_an_orphan = false;
    while Instant::now() < deadline {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(_, 3)) => {
                reaped_an_orphan = true;
                break;
            }
            Ok(WaitStatus::StillAlive) | Err(nix::errno::Errno::ECHILD) => {
                std::thread::sleep(Duration::from_millis(20));
            }
            other => {
                panic!("unexpected waitpid result while waiting for the grandchild: {other:?}")
            }
        }
    }
    assert!(
        reaped_an_orphan,
        "the grandchild must have been reaped by this subreaper"
    );
}
