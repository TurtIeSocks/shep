//! Behavioral tests for [`shep_daemon::tokio_runner::TokioRunner`] against
//! real Windows child processes.
//!
//! Windows has no signal delivery, so most cases here assert properties
//! the Windows runner claims instead. The load-bearing one is job
//! containment: a sheep outside its job is one `kill_tree` cannot reach,
//! so `shep stop` would report success while leaving a process running.

#![cfg(windows)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use shep_daemon::runner::{ProcessRunner, RunningProcess, SpawnSpec, StopSignal};
use shep_daemon::tokio_runner::TokioRunner;

/// A child that stays alive far longer than any test here, with its stdio
/// redirected.
///
/// `ping`, not `timeout /t`: `timeout.exe` refuses to run when stdin is
/// not a console, and the runner gives every sheep a null stdin, so it
/// would otherwise exit instantly instead of sleeping.
const LONG_RUNNING: [&str; 4] = ["ping", "-n", "600", "127.0.0.1"];

/// How long a "did it actually die" assertion waits before failing:
/// generous, since a loaded CI box is slower than a quiet laptop.
const SETTLE: Duration = Duration::from_secs(15);

/// The environment a real sheep gets, in miniature.
///
/// Not empty: the runner's `env_clear()` means an empty environment
/// breaks `cmd`/`powershell` on Windows. `assemble`'s private `base_env`
/// normally supplies this floor; a hand-built spec must reproduce it.
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
/// Polls rather than reading once: the log pump writes on its own task, so
/// a single read right after the child exits can race it.
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
    // An empty log says nothing arrived, not why. A sheep that failed to
    // launch reports it on stderr, in a sibling file this does not check.
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
/// Read from the process table rather than having the sheep report its own
/// child's pid, which would need PowerShell. Callers `expect` this: a
/// grandchild it cannot find fails the test rather than skipping the
/// assertion.
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

/// fails if a real child's stdout never reaches its log file.
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
/// Also pins the exit code: Windows carries no signal number, so `137` is
/// what distinguishes a shep-killed sheep from one that exited on its own
/// in `ProcessInfo::last_exit` and the `EXIT` column.
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

/// fails if a sheep's own child survives `kill_tree`, the reason the
/// runner creates a job object at all.
///
/// The sheep is a `cmd` batch that launches a background `ping` with
/// `start /b` and waits, giving `kill_tree` a grandchild to contain.
#[tokio::test]
async fn kill_tree_reaches_a_grandchild_and_not_just_the_sheep() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = cmd_spec(&dir, &["echo", "placeholder"]);
    // `cmd` from a batch file, not `powershell -Command`: PowerShell hangs
    // this suite.
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
/// The refusal message names the supported path, so an operator whose
/// `shep stop` ends in a `kill_timeout` termination can find the reason
/// in the daemon log.
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
/// The fixture is a `.cmd` file, not `cmd /C <script>`: `std::process::Command`
/// escapes an argument's inner quotes MSVC-style, which `cmd.exe` reads
/// literally, breaking a quoted redirect target. A script file's contents
/// go through no such escaping.
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
