//! Drives the PID-1 init split as a real process.
//! `commands::reap`'s signal forwarding and relayed exit status have no
//! coverage elsewhere.
//!
//! `#![cfg(unix)]`: `commands` itself is unix only. An integration test
//! file is its own compilation unit. Without the guard, `--all-targets`
//! would build this on Windows.
//!
//! Every case sets `SHEP_FORCE_INIT=1`: a test harness is never PID 1.
//! Without it, `should_split` never fires. See
//! `commands::reap::should_split`.

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

/// Bound on every wait in this file: reading the init's pid line, the
/// flock coming online, and the process's exit after a signal. Generous
/// headroom for a loaded CI boot, not a protocol timeout.
const INIT_DEADLINE: Duration = Duration::from_secs(10);

/// Writes an executable script into `dir` and returns its path. The
/// executable bit matters: without it `shep runtime` fails EACCES.
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

/// A long-lived, single-app Flockfile: a bare `sleep 60`. It dies on
/// the first `SIGTERM`, its default disposition, needing no readiness
/// handshake.
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

/// Spawns `shep runtime <flockfile>` with `$SHEP_FORCE_INIT=1`. Stdout
/// and stderr are both piped so neither fills and blocks the child.
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

/// Reads `source` line by line on a background thread, forwarding lines
/// onto the returned channel. Lets the caller bound the wait
/// ([`recv_pid_line`]) rather than blocking on the pipe.
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
/// streams bleats to its own stdout continuously. An undrained pipe
/// fills and blocks the child, wedging the test.
fn discard_in_background<R: Read + Send + 'static>(mut source: R) {
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut source, &mut std::io::sink());
    });
}

/// Blocks on `rx` until a line matching `shep runtime: init supervising
/// pid <N>` arrives, and returns `N`. Panics past `deadline`: absence
/// within a generous bound means the process never got that far.
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

/// Polls `shep --home <home> flock --format json` until the one sheep's
/// status is `online`, or `deadline` elapses. Reaches the same daemon
/// `shep runtime` booted in-process, over the socket bound at `home`.
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

/// Polls `child.try_wait()` until it exits, or `timeout` elapses. A
/// named panic here, rather than the harness's own process timeout,
/// which would fail the whole binary and name nothing.
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

/// Best-effort cleanup so a panicking assertion never leaves a real
/// daemon or sleeping sheep behind: SIGKILLs the supervisor, if known,
/// then the init.
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

/// Fails if the init ignores `SIGTERM` or exits with its own status
/// instead of the supervisor's.
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

/// Reads the process's real exit status rather than `classify`'s return
/// value: `ExitCode` has no variant for 137.
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

/// Fails if the drain loop leaves a reparented orphan as a zombie. The
/// real failure mode is a process table that fills over days.
///
/// Linux only: `PR_SET_CHILD_SUBREAPER` makes this process the reaper
/// for its own descendants; macOS has no equivalent.
///
/// Does not go through the `shep` binary: `lib.rs` exposes only three
/// functions, none of `commands::reap`. Proves instead the mechanism
/// `commands::reap::drain` relies on. A bounded `waitpid(-1, WNOHANG)`
/// loop, run by a subreaper, reaps a reparented grandchild.
#[cfg(target_os = "linux")]
#[test]
fn a_reparented_orphan_is_reaped() {
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};

    nix::sys::prctl::set_child_subreaper(true).expect("Linux has supported this since 3.4");

    // Targets this one pid rather than `-1`: every `#[test]` runs as a
    // thread in one shared process, so a wildcard wait can reap another
    // test's child.
    let mut shell = std::process::Command::new("/bin/sh")
        .args(["-c", "(sleep 0.2; exit 3) & echo $!"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the shell");
    let mut printed_pid = String::new();
    BufReader::new(shell.stdout.take().expect("shell stdout was piped"))
        .read_line(&mut printed_pid)
        .expect("read the grandchild's pid off the shell's stdout");
    let _ = shell.wait();
    let grandchild_pid = Pid::from_raw(
        printed_pid
            .trim()
            .parse()
            .unwrap_or_else(|e| panic!("shell printed no valid pid ({e}): {printed_pid:?}")),
    );

    // Bounded drain: never `loop {}` against a real kernel event.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut reaped_an_orphan = false;
    while Instant::now() < deadline {
        match waitpid(grandchild_pid, Some(WaitPidFlag::WNOHANG)) {
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
