//! Behavioral tests for [`shep_daemon::tokio_runner::TokioRunner`] against
//! real Windows child processes.
//!
//! The Windows counterpart to `real_runner.rs`, which is `#![cfg(unix)]` and
//! built entirely on `/bin/sh` scripts. This file is deliberately NOT a
//! translation of that one: most of its cases turn on signal delivery, and
//! the Windows tier's honest answer to a graceful signal is a refusal. What
//! is asserted here instead is the set of properties the Windows runner
//! genuinely claims, each of which would be a silent, dangerous failure if
//! it did not hold.
//!
//! The load-bearing one is job containment. A sheep outside its job is a
//! sheep `kill_tree` cannot reach, so `shep stop` would report success and
//! leave a process running — the worst failure mode a supervisor has.

#![cfg(windows)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use shep_daemon::runner::{ProcessRunner, RunningProcess, SpawnSpec, StopSignal};
use shep_daemon::tokio_runner::TokioRunner;

/// A child that stays alive far longer than any test here, and that keeps
/// working with its stdio redirected.
///
/// `ping`, not the more obvious `timeout /t`: `timeout.exe` refuses to run at
/// all when stdin is not a console ("ERROR: Input redirection is not
/// supported"), and the runner gives every sheep a null stdin. It exits
/// instantly instead of sleeping, which silently turns a containment test
/// into a test of nothing — measured, not guessed.
const LONG_RUNNING: [&str; 4] = ["ping", "-n", "600", "127.0.0.1"];

/// How long a "did it actually die" assertion waits before failing. Generous:
/// a loaded CI box terminating a process tree is slower than a quiet laptop,
/// and every use below resolves far sooner in the normal case.
const SETTLE: Duration = Duration::from_secs(15);

/// The environment a real sheep gets, in miniature.
///
/// NOT `BTreeMap::new()`, and the difference is the whole reason this helper
/// exists. The runner calls `env_clear()` before applying `SpawnSpec::env`,
/// so an empty map means a child with a genuinely empty environment — which
/// on Windows is not "a clean child", it is a broken one: `powershell`
/// launched that way produces no output and no error at all. `assemble`'s
/// `base_env` is what saves a real sheep from this, and it is private, so a
/// spec built by hand for a test has to reproduce its floor or it is testing
/// a configuration shep never actually produces.
fn realistic_env() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for key in [
        "PATH",
        "SystemRoot",
        "windir",
        "SystemDrive",
        "COMSPEC",
        "PATHEXT",
        "TEMP",
    ] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.to_string(), value);
        }
    }
    env
}

/// Builds a `cmd /C <args>` spec writing logs into `dir`.
fn cmd_spec(dir: &tempfile::TempDir, args: &[&str]) -> SpawnSpec {
    let mut argv = vec!["/C".to_string()];
    argv.extend(args.iter().map(|a| (*a).to_string()));
    SpawnSpec {
        name: "web".to_string(),
        program: "cmd".to_string(),
        args: argv,
        cwd: Some(dir.path().to_path_buf()),
        env: realistic_env(),
        out_file: dir.path().join("web-out.log"),
        err_file: dir.path().join("web-err.log"),
        channel: false,
        stdin: false,
        credentials: None,
    }
}

/// Reads `path` until it contains `needle`, or fails after [`SETTLE`].
///
/// Polls rather than reading once: the log pump writes on its own task, so a
/// single read immediately after the child exits is a race the test would
/// lose intermittently rather than consistently — the worst kind.
async fn wait_for_log(path: &PathBuf, needle: &str) -> String {
    let started = tokio::time::Instant::now();
    let deadline = tokio::time::Instant::now() + SETTLE;
    let mut last = String::new();
    while tokio::time::Instant::now() < deadline {
        last = std::fs::read_to_string(path).unwrap_or_default();
        if last.contains(needle) {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // An empty log says only that nothing arrived, not why. On the CI runner
    // this fired with `last saw ""`, which left the interesting half unasked:
    // a sheep that failed to launch at all reports it on stderr, and the
    // runner writes that to a sibling file nobody was reading.
    let sibling = path.with_file_name(path.file_name().and_then(|name| name.to_str()).map_or_else(
        || "web-err.log".to_string(),
        |name| name.replace("out", "err"),
    ));
    panic!(
        "{needle:?} never reached {} after {:?}; last saw {last:?}\n\
         out file exists: {}, len {:?}\n\
         stderr file {}: {:?}",
        path.display(),
        started.elapsed(),
        path.exists(),
        std::fs::metadata(path).map(|m| m.len()).ok(),
        sibling.display(),
        std::fs::read_to_string(&sibling).ok(),
    );
}

/// Whether `pid` names a live process right now.
fn pid_is_alive(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate};
    let mut system = sysinfo::System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );
    system.process(Pid::from_u32(pid)).is_some()
}

/// The pid of a live child of `parent`, or `None`.
///
/// Replaces having the sheep print its own child's pid, which needed a
/// shell that could ask Windows for it, which meant PowerShell, which is
/// what hung this suite for four CI runs. This asks the same question from
/// the test process, where an answer of `None` is a visible failure rather
/// than a wait that never ends.
///
/// The concern that led to the sheep naming its own child was that
/// `sysinfo` might not SEE a grandchild that `Win32_Process` showed, which
/// would make a containment test pass while finding nothing. That is why
/// the caller `expect`s this: a grandchild it cannot find fails the test
/// rather than skipping the assertion.
fn child_of(parent: u32) -> Option<u32> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate};
    let mut system = sysinfo::System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::everything(),
    );
    system
        .processes()
        .values()
        .find(|process| process.parent() == Some(Pid::from_u32(parent)))
        .map(|process| process.pid().as_u32())
}

/// fails if a real child's stdout never reaches its log file. The most basic
/// thing the runner does, and the one every other case here depends on.
#[tokio::test]
async fn a_real_child_writes_its_stdout_to_the_log_file() {
    let dir = tempfile::tempdir().unwrap();
    let spec = cmd_spec(&dir, &["echo", "hello-from-windows"]);
    let runner = TokioRunner::new();

    let (mut proc, _io) = runner.spawn(&spec).unwrap();
    let outcome = proc.wait().await;

    assert_eq!(outcome.code, Some(0), "a plain echo must exit cleanly");
    assert_eq!(
        outcome.signal, None,
        "a Windows exit carries no signal number, ever"
    );
    let logged = wait_for_log(&spec.out_file, "hello-from-windows").await;
    assert!(logged.contains("hello-from-windows"), "{logged}");
}

/// fails if `kill_tree` does not stop the sheep itself.
///
/// Also pins the exit code, which matters more on Windows than it looks:
/// there is no signal number for a listing to show, so `137` is the ONLY
/// thing distinguishing "shep killed this" from "it exited on its own" in
/// `ProcessInfo::last_exit` and the `EXIT` column.
#[tokio::test]
async fn kill_tree_stops_a_long_running_sheep_and_reports_a_recognisable_code() {
    let dir = tempfile::tempdir().unwrap();
    // ~600s of pinging, so an exit inside this test can only be the kill.
    let spec = cmd_spec(&dir, &LONG_RUNNING);
    let runner = TokioRunner::new();

    let (mut proc, _io) = runner.spawn(&spec).unwrap();
    proc.kill_tree().expect("kill_tree must reach a live sheep");

    let outcome = tokio::time::timeout(SETTLE, proc.wait())
        .await
        .expect("a killed sheep must not outlive its kill_tree");
    assert_eq!(
        outcome.code,
        Some(137),
        "a shep-killed sheep must report 128+9, the same number the unix tier shows"
    );
}

/// fails if a sheep's own child survives `kill_tree` — the whole reason the
/// runner creates a job object at all.
///
/// The sheep is a `cmd` batch that launches a background `ping` with
/// `start /b` and then waits, so there is a grandchild to contain and a
/// parent for `kill_tree` to address.
///
/// The grandchild is found by walking the process table
/// (`sysinfo`'s `parent()` reports the relationship reliably; `Win32_Process`
/// would too, but reaching it needs a shell able to ask Windows for a pid,
/// which means PowerShell — and PowerShell is what hangs this suite, see
/// below). `child_of` is `expect`ed, so a grandchild the test cannot see
/// fails it rather than skipping the assertion and looking green.
///
/// This is the assertion that would go red if `spawn` ever stopped assigning
/// the child to its job — a change that breaks nothing else, and that every
/// other test in this file would keep passing through.
#[tokio::test]
async fn kill_tree_reaches_a_grandchild_and_not_just_the_sheep() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = cmd_spec(&dir, &["echo", "placeholder"]);
    // `cmd` from a batch file, not `powershell -Command`: a PowerShell
    // fixture here hangs the suite, with both log files present and empty.
    //
    // `start /b` is the grandchild: a process the sheep spawns that
    // outlives it, which is the whole point of the case. The sheep then
    // waits, so `kill_tree` has something to kill.
    const CRLF: &str = "\r\n";
    let script = dir.path().join("lamb.cmd");
    std::fs::write(
        &script,
        [
            "@echo off",
            "start /b ping -n 60 127.0.0.1 >nul",
            "echo LAMB-STARTED",
            "ping -n 60 127.0.0.1 >nul",
            "",
        ]
        .join(CRLF),
    )
    .expect("the lamb fixture script must be writable");
    spec.program = "cmd".to_string();
    spec.args = vec!["/C".to_string(), script.display().to_string()];
    let runner = TokioRunner::new();

    let (mut proc, _io) = runner.spawn(&spec).unwrap();
    // The batch says when it has started its grandchild; the pid itself
    // comes from the process table, since `cmd` cannot report one.
    wait_for_log(&spec.out_file, "LAMB-STARTED").await;
    let sheep = proc.pid();
    let lamb = child_of(sheep).expect(
        "the sheep's grandchild must be visible in the process table, or this \
         case proves nothing about containment",
    );

    assert!(
        pid_is_alive(lamb),
        "grandchild {lamb} must be live before the kill, or this proves nothing"
    );

    proc.kill_tree().unwrap();
    let _ = tokio::time::timeout(SETTLE, proc.wait()).await;

    let deadline = tokio::time::Instant::now() + SETTLE;
    while pid_is_alive(lamb) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "grandchild {lamb} outlived kill_tree: the job object is not containing the tree"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// fails if the graceful rung pretends to have delivered something.
///
/// The refusal is the contract (see `TokioProc::signal`'s Windows arm), and
/// the message is part of it: an operator whose `shep stop` took the full
/// `kill_timeout` and ended in a termination needs the reason to be findable,
/// and this string in the daemon log is where it is findable. An arm that
/// returned `Ok(())` would leave the ladder believing a polite stop landed
/// and would make the whole platform difference invisible.
#[tokio::test]
async fn the_graceful_rung_refuses_honestly_and_names_the_way_out() {
    let dir = tempfile::tempdir().unwrap();
    let spec = cmd_spec(&dir, &LONG_RUNNING);
    let runner = TokioRunner::new();

    let (mut proc, _io) = runner.spawn(&spec).unwrap();
    let refusal = proc
        .signal(StopSignal::Term)
        .expect_err("Windows must not claim to have delivered a graceful signal");

    let message = refusal.to_string();
    assert!(
        message.contains("shutdown_with_message"),
        "the refusal must name the supported graceful path, got {message:?}"
    );

    proc.kill_tree().unwrap();
    let _ = tokio::time::timeout(SETTLE, proc.wait()).await;
}

/// fails if a child cannot reach the shepherd channel by pipe name.
///
/// Isolates the channel from everything above it: no daemon, no
/// `wait_ready`, just the runner's own `SHEP_CHANNEL_PIPE` and the pumps
/// behind `ProcIo::from_child`.
///
/// **The fixture is a `.cmd` FILE rather than `cmd /C <script>`, and that is
/// load-bearing.** `std::process::Command` escapes an argument's inner
/// quotes as `\"`, which is the MSVC C runtime's convention and NOT
/// `cmd.exe`'s — cmd takes the backslash literally, so a redirect target
/// arrives as `\"C:\...\"` and fails with "The filename, directory name, or
/// volume label syntax is incorrect". Measured, after two wrong guesses.
/// A script file's CONTENTS go through no such escaping.
#[tokio::test]
async fn a_child_reaches_the_shepherd_channel_by_pipe_name() {
    let dir = tempfile::tempdir().unwrap();
    let seen = dir.path().join("seen.txt");
    let script = dir.path().join("channel.cmd");
    // Joined with escaped CRLF: a `.cmd` wants it, and the .rs file itself
    // is pinned to LF, so a literal CR here would not survive.
    let body = [
        "@echo off".to_string(),
        format!("(echo %SHEP_CHANNEL_PIPE%) > \"{}\"", seen.display()),
        "(echo {\"kind\":\"ready\"}) > \"%SHEP_CHANNEL_PIPE%\"".to_string(),
        "ping -n 30 127.0.0.1 >nul".to_string(),
        String::new(),
    ]
    .join("\r\n");
    std::fs::write(&script, body).unwrap();

    let mut spec = cmd_spec(&dir, &["echo", "placeholder"]);
    spec.program = script.display().to_string();
    spec.args = Vec::new();
    spec.channel = true;

    let runner = TokioRunner::new();
    let (mut proc, mut io) = runner.spawn(&spec).unwrap();

    let got = tokio::time::timeout(SETTLE, io.from_child.recv()).await;
    let saw = std::fs::read_to_string(&seen).unwrap_or_else(|_| "<file never written>".into());
    let child_err = std::fs::read_to_string(&spec.err_file).unwrap_or_default();
    proc.kill_tree().unwrap();
    let _ = tokio::time::timeout(SETTLE, proc.wait()).await;

    let message = got.unwrap_or_else(|_| {
        panic!(
            "no channel message within {SETTLE:?}; child saw SHEP_CHANNEL_PIPE={saw:?};              child stderr={child_err:?}"
        )
    });
    assert!(
        message.is_some(),
        "the channel closed without delivering; child saw SHEP_CHANNEL_PIPE={saw:?}"
    );
}
