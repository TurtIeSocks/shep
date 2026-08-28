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
/// The sheep is a PowerShell that launches a background `ping` and prints
/// its pid, so the grandchild identifies itself through the log pump. That
/// is deliberately not done by walking the process table: `sysinfo`'s
/// `parent()` did not reliably report the relationship on this platform
/// (measured — `Win32_Process` showed the child while `sysinfo` did not), and
/// a containment test that silently fails to FIND the grandchild proves
/// nothing while looking like it passed. Having the sheep name its own child
/// removes the question.
///
/// This is the assertion that would go red if `spawn` ever stopped assigning
/// the child to its job — a change that breaks nothing else, and that every
/// other test in this file would keep passing through.
// Ran ignored on Windows for four commits. On the runner this failed with
// both log files present and ZERO bytes, so the sheep launched and wrote
// nothing to either stream inside the deadline. The script above now
// flushes stdout explicitly, which is the one explanation that fits an
// empty stdout AND an empty stderr from a process that then sleeps for a
// minute without exiting.
//
// If that is wrong, the panic now says how long it waited, which separates
// a slow start from a line that never comes.
#[tokio::test]
async fn kill_tree_reaches_a_grandchild_and_not_just_the_sheep() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = cmd_spec(&dir, &["echo", "placeholder"]);
    spec.program = "powershell".to_string();
    spec.args = vec![
        "-NoProfile".to_string(),
        "-Command".to_string(),
        // `[Console]::Out`, not `Write-Output`: this script prints one
        // line and then sleeps for a minute, so anything PowerShell
        // buffers on a redirected stdout does not reach the log until
        // long after the test has given up waiting for it. The explicit
        // flush is the whole point; the rest is the same script.
        concat!(
            "$p = Start-Process ping -ArgumentList '-n','60','127.0.0.1' ",
            "-PassThru -WindowStyle Hidden; ",
            "[Console]::Out.WriteLine('LAMB=' + $p.Id); [Console]::Out.Flush(); ",
            "Start-Sleep -Seconds 60"
        )
        .to_string(),
    ];
    let runner = TokioRunner::new();

    let (mut proc, _io) = runner.spawn(&spec).unwrap();
    let logged = wait_for_log(&spec.out_file, "LAMB=").await;
    let lamb: u32 = logged
        .lines()
        .find_map(|line| line.trim().strip_prefix("LAMB="))
        .expect("the sheep must report its grandchild")
        .trim()
        .parse()
        .expect("a pid");

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

/// Writes one line where a **passing** test's output is actually readable.
///
/// libtest swallows `println!` unless `--nocapture` is passed — it swaps the
/// thread-local buffer `print!` writes into — and it only replays what it
/// swallowed for tests that FAIL. Neither half suits the report below, which
/// is an instrument: it has no finding it is willing to fail over, and it is
/// most wanted on the run where nobody thought to pass a flag.
///
/// `Stdout` itself is not part of that capture machinery — only the `print!`
/// family consults the capture buffer — so a direct `write_all` reaches the
/// real handle in both modes, and under `--nocapture` is byte-for-byte what
/// `println!` would have produced. Measured both ways on 2026-08-27 rather
/// than reasoned about, because it is an implementation detail of libtest and
/// a wrong guess would silently produce a test that reports nothing.
fn report(line: &str) {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Renders an `io::Result` step so a failure shows its exact OS error.
///
/// `Debug` rather than `Display` on the error deliberately: `Display` gives
/// "Access is denied. (os error 5)" while `Debug` gives the `ErrorKind` and
/// the raw code as separate, greppable fields, and a number is what a reader
/// comparing two machines will actually search for.
fn step<T: std::fmt::Debug>(outcome: Option<&std::io::Result<T>>) -> String {
    match outcome {
        None => "not attempted".to_string(),
        Some(Ok(value)) => format!("ok: {value:?}"),
        Some(Err(error)) => format!("FAILED: {error:?}"),
    }
}

/// Reports — and never asserts — why job containment might behave differently
/// here than on another machine.
///
/// **This test is an instrument, not a claim.** It passes whatever it finds,
/// because everything it finds is a fact about the machine running it rather
/// than about shep. Two tests that pass on a developer's Windows box fail or
/// hang on GitHub's `windows-latest` runner
/// (`kill_tree_reaches_a_grandchild_and_not_just_the_sheep` here, and
/// `daemon_e2e.rs`'s `reopen_moves_a_running_sheeps_log_onto_the_recreated_path`),
/// and the leading hypothesis is that the runner has already put the job at
/// the top of shep's tree inside a job of its own. A job created inside
/// another is a *nested* job: the outer job's limit flags constrain it, and
/// `JOB_OBJECT_LIMIT_BREAKAWAY_OK` / `..._SILENT_BREAKAWAY_OK` /
/// `..._KILL_ON_JOB_CLOSE` / an `ActiveProcessLimit` each change what the
/// inner one can do.
///
/// So it prints the outer job's decoded limits, runs `tokio_runner`'s exact
/// containment sequence against a real tree, and prints which pids the nested
/// job actually contained. Run it on both machines and diff the output; the
/// difference is the answer.
///
/// The tree is short-lived on purpose — a 20-count `ping`, roughly twenty
/// seconds — and is killed by pid at the end regardless of what the job did,
/// so a machine where containment fails still gets a clean exit rather than
/// an orphan holding a CI step open. That failure mode is precisely what
/// forced `kill_tree_reaches_a_grandchild_and_not_just_the_sheep` to be
/// ignored, and this test must not reproduce it.
#[tokio::test]
async fn job_object_environment_reports_itself() {
    use shep_daemon::sys_windows;

    report("");
    report("=== shep windows job-object environment report =======================");

    let environment = sys_windows::job_environment();
    match environment.version {
        Ok(version) => report(&format!(
            "windows version (RtlGetVersion): {version}  [major={} minor={} build={}]",
            version.major, version.minor, version.build
        )),
        Err(status) => report(&format!(
            "windows version (RtlGetVersion): FAILED, NTSTATUS {status:#010x}"
        )),
    }

    match &environment.in_job {
        Ok(true) => report("already inside a job object: YES"),
        Ok(false) => report("already inside a job object: no"),
        Err(error) => report(&format!("already inside a job object: UNKNOWN, {error:?}")),
    }

    match &environment.outer_limits {
        None => report("outer job limits: n/a (no enclosing job to query)"),
        Some(Err(error)) => report(&format!("outer job limits: query FAILED, {error:?}")),
        Some(Ok(limits)) => {
            report(&format!(
                "outer job LimitFlags: {:#010x}",
                limits.limit_flags
            ));
            if limits.named_flags.is_empty() {
                report("outer job flags by name: (none set)");
            } else {
                for name in &limits.named_flags {
                    report(&format!("outer job flag set: {name}"));
                }
            }
            // Spelled out one by one, including the absent ones. A reader
            // diffing two runs needs "no" to be present rather than inferred
            // from a name's absence in a list.
            for name in [
                "JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE",
                "JOB_OBJECT_LIMIT_BREAKAWAY_OK",
                "JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK",
                "JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION",
            ] {
                let set = limits.named_flags.contains(&name);
                report(&format!(
                    "outer job {name}: {}",
                    if set { "SET" } else { "not set" }
                ));
            }
            report(&format!(
                "outer job ActiveProcessLimit: {}",
                limits
                    .active_process_limit
                    .map_or_else(|| "not limited".to_string(), |n| n.to_string())
            ));
            report(&format!(
                "outer job JobMemoryLimit: {}",
                limits
                    .job_memory_limit
                    .map_or_else(|| "not limited".to_string(), |n| format!("{n} bytes"))
            ));
            report(&format!(
                "outer job ProcessMemoryLimit: {}",
                limits
                    .process_memory_limit
                    .map_or_else(|| "not limited".to_string(), |n| format!("{n} bytes"))
            ));
        }
    }

    report("--- nested job: create, assign, inspect, terminate --------------------");

    // The same shape as `kill_tree_reaches_a_grandchild_and_not_just_the_sheep`
    // — a PowerShell that launches a background `ping` and names its pid — but
    // spawned directly rather than through `TokioRunner`, so the job created
    // below is the FIRST level nested inside whatever this machine already
    // provides, exactly where the runner's own job sits. Going through the
    // runner would add a third level and measure something else.
    //
    // 20 pings, not 600: a machine where none of this works keeps the stray
    // process for twenty seconds rather than ten minutes.
    let mut child = tokio::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "$p = Start-Process ping -ArgumentList '-n','20','127.0.0.1' -PassThru \
             -WindowStyle Hidden; Write-Output ('LAMB=' + $p.Id); Start-Sleep -Seconds 20",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn()
        .expect("powershell must be spawnable on Windows");

    let sheep_pid = child.id();
    report(&format!("sheep pid: {sheep_pid:?}"));

    // Assigned HERE, before the sheep has had time to create anything. Job
    // membership propagates only to processes created after the assignment,
    // so a probe that waited for the grandchild's pid first would find it
    // outside the job on every machine on earth and prove nothing.
    let probe = child
        .raw_handle()
        .map(sys_windows::probe_nested_job)
        .expect("a just-spawned child has a raw handle");
    report(&format!(
        "nested CreateJobObjectW: {}",
        step(Some(&probe.create))
    ));
    report(&format!(
        "nested AssignProcessToJobObject: {}",
        step(probe.assign.as_ref())
    ));

    let lamb = read_lamb_pid(&mut child).await;
    report(&format!("grandchild pid reported by the sheep: {lamb:?}"));

    match probe.members() {
        None => report("nested job members: n/a (no job)"),
        Some(Err(error)) => report(&format!("nested job members: query FAILED, {error:?}")),
        Some(Ok(members)) => {
            report(&format!(
                "nested job NumberOfAssignedProcesses: {}",
                members.assigned
            ));
            report(&format!("nested job member pids: {:?}", members.pids));
            report(&format!(
                "nested job contains the sheep: {}",
                sheep_pid.is_some_and(|pid| members.pids.contains(&pid))
            ));
            report(&format!(
                "nested job contains the grandchild: {}",
                lamb.is_some_and(|pid| members.pids.contains(&pid))
            ));
        }
    }

    report(&format!(
        "nested TerminateJobObject: {}",
        step(probe.terminate().as_ref())
    ));

    // Five seconds, not SETTLE: this reports rather than asserts, so a long
    // wait buys nothing but a slower suite. A machine that needs longer than
    // this to reap a terminated job is itself a finding worth printing.
    let survivors = survivors_after(Duration::from_secs(5), sheep_pid, lamb).await;
    report(&format!(
        "sheep alive 5s after TerminateJobObject: {}",
        survivors.0
    ));
    report(&format!(
        "grandchild alive 5s after TerminateJobObject: {}",
        survivors.1
    ));

    // Unconditional cleanup, whatever the job did or did not manage. This is
    // the whole reason the test can stay un-ignored: nothing it spawned is
    // allowed to outlive it and hold a CI step open.
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    for pid in [sheep_pid, lamb].into_iter().flatten() {
        if pid_is_alive(pid) {
            taskkill(pid);
        }
    }
    report("======================================================================");
    report("");
}

/// Reads the `LAMB=<pid>` line the sheep prints, or `None` if it never comes.
///
/// Bounded, and returning `None` rather than panicking on a timeout: a sheep
/// that never reports its grandchild is a finding this test prints, not a
/// reason to fail.
async fn read_lamb_pid(child: &mut tokio::process::Child) -> Option<u32> {
    use tokio::io::{AsyncBufReadExt as _, BufReader};

    let stdout = child.stdout.take()?;
    let mut lines = BufReader::new(stdout).lines();
    let found = tokio::time::timeout(SETTLE, async {
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(pid) = line.trim().strip_prefix("LAMB=") {
                return pid.trim().parse::<u32>().ok();
            }
        }
        None
    })
    .await;
    found.ok().flatten()
}

/// Whether each of `sheep` and `lamb` is still alive after `grace`.
///
/// Polls so a prompt death is reported promptly; the full wait is only paid
/// when something genuinely survived.
async fn survivors_after(
    grace: Duration,
    sheep: Option<u32>,
    lamb: Option<u32>,
) -> (String, String) {
    let deadline = tokio::time::Instant::now() + grace;
    let alive = |pid: Option<u32>| pid.is_some_and(pid_is_alive);
    while tokio::time::Instant::now() < deadline && (alive(sheep) || alive(lamb)) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let describe = |pid: Option<u32>| match pid {
        None => "unknown (pid never observed)".to_string(),
        Some(pid) if pid_is_alive(pid) => format!("YES — {pid} SURVIVED"),
        Some(pid) => format!("no — {pid} is gone"),
    };
    (describe(sheep), describe(lamb))
}

/// Kills `pid` and its descendants outright, best effort.
///
/// The safety net behind the report: whatever the job object did or did not
/// contain, nothing this test spawned outlives it.
fn taskkill(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
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
