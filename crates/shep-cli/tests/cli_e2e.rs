//! End-to-end tier: drives the real `shep` binary via `assert_cmd` against a
//! real daemon, a real socket, and real spawned sheep, each on a fresh
//! `$SHEP_HOME` in its own [`tempfile::TempDir`].
//!
//! This is the first tier where the whole stack runs as an actual binary
//! rather than through the unit tiers' fakes — everything the fakes could
//! not reach (autostart, the cold-start race, real exit codes, real stderr
//! vs stdout separation, the daemon's own process-group leadership) lives
//! here.
//!
//! `#![cfg(unix)]`: an integration test file is its own compilation unit, so
//! without this, `--all-targets` plus `cargo test --workspace` would build
//! it (with its unix-only `nix` dev-dependency and
//! `std::os::unix::fs::PermissionsExt` usage) on the Windows CI leg too.
//! Global Constraints names this file explicitly for that reason.
//!
//! Every case's command chain carries `.timeout(CMD_TIMEOUT)` before
//! `.output()`, so a regression that hangs (case 7's `--no-follow`
//! following forever being the live hazard) fails as a named assertion
//! instead of a killed CI job. Every case that can leave a daemon behind
//! registers its `$SHEP_HOME` with a [`DaemonGuard`] immediately after the
//! `Output` that might have spawned one, before any assertion that could
//! panic.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use tempfile::TempDir;

/// Bound on every `shep` invocation in this file. `assert_cmd`'s
/// `.output()` blocks unbounded without it; case 7 (`bleats --no-follow`)
/// is the live hazard, since its regression mode is following forever.
const CMD_TIMEOUT: Duration = Duration::from_secs(30);

/// How long [`bleats_no_follow_until_written`] keeps retrying.
const BLEATS_DEADLINE: Duration = Duration::from_secs(10);

/// Gap between [`bleats_no_follow_until_written`]'s retries.
const BLEATS_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long a fixture sheep's script sleeps after writing whatever it
/// writes. Long enough that no case in this file could plausibly outlast it
/// (every case finishes in well under a second of real daemon/sheep work);
/// short enough that a sheep orphaned by a panicking test (see
/// [`DaemonGuard`]'s own doc on the one case it cannot close) self-terminates
/// quickly rather than lingering for the rest of a CI job.
const SCRIPT_SLEEP_SECS: u32 = 60;

// --- Fixture helpers ---------------------------------------------------

/// The path of the committed `--format json` fixture named `name`.
fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{name}.json"))
}

/// Loads and parses a committed fixture. Every envelope fixture is compared
/// as a `serde_json::Value` (structural equality, not byte equality) since
/// `normalize_process_info`/`normalize_ping` already reduce the real output
/// to the same shape; only `bleats_no_follow.json` (case 4's second half) is
/// compared byte-for-byte, directly against `std::fs::read`, not through
/// this function.
fn load_fixture(name: &str) -> serde_json::Value {
    let path = fixture_path(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Writes a trivial long-running script into `dir` and returns its path.
/// The executable bit is the point: without `set_mode(0o755)` every
/// `shep start` fails EACCES and every case that starts a sheep fails
/// together, for a reason that has nothing to do with the CLI.
///
/// `exec sleep`, not a bare `sleep`: verified empirically (`ps` before/after
/// a real `shep kill`) that a bare trailing `sleep` is a *forked* child of
/// the `/bin/sh` process the daemon actually tracks, in the shell's own
/// process group. The daemon's graceful stop signals only the one recorded
/// pid (`shep-daemon/src/tokio_runner.rs`'s `signal`, not its group-wide
/// `kill_tree`), which kills the shell and orphans that untracked `sleep`
/// grandchild — reparented, still running, invisible to both `shep kill`
/// and to [`DaemonGuard`]. `exec` replaces the shell's own process image
/// with `sleep`, in place (same pid, same group), so there is only ever one
/// process to track and signal.
fn write_test_script(dir: &TempDir) -> PathBuf {
    write_script(
        dir,
        "sheep.sh",
        &format!("#!/bin/sh\nexec sleep {SCRIPT_SLEEP_SECS}\n"),
    )
}

/// Writes a script that emits one marker line on stdout, optionally one on
/// stderr, and then sleeps. Same `0o755` requirement, and the same `exec`
/// requirement, as [`write_test_script`] — the `echo` lines still run as
/// ordinary steps of the shell process; only the final long-running command
/// replaces it.
///
/// `None` writes to stderr not at all — not an empty line. An empty line is
/// still a line: it reaches the err file, `--no-follow` renders it, and
/// case 4's byte-exact fixture gains a second object it did not predict.
///
/// The sleep is what makes the output countable: a script that exits is
/// restarted, and each restart appends another copy of every marker, so a
/// byte-exact fixture would stop being byte-exact after the first respawn.
fn write_logging_script(dir: &TempDir, out_marker: &str, err_marker: Option<&str>) -> PathBuf {
    let mut script = format!("#!/bin/sh\necho '{out_marker}'\n");
    if let Some(err_marker) = err_marker {
        script.push_str(&format!("echo '{err_marker}' 1>&2\n"));
    }
    script.push_str(&format!("exec sleep {SCRIPT_SLEEP_SECS}\n"));
    write_script(dir, "logging.sh", &script)
}

/// Shared write-plus-chmod tail of both script helpers above.
fn write_script(dir: &TempDir, name: &str, contents: &str) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, contents).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

// --- Command helpers -----------------------------------------------------

/// A `shep --home <home>` invocation, timeout already attached. Every case
/// below appends its own verb and flags, then `.output()`s it.
fn shep(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("shep").unwrap();
    cmd.arg("--home").arg(home).timeout(CMD_TIMEOUT);
    cmd
}

/// Asserts `output` exited `Success`, printing stderr on failure so a
/// red run names the actual cause instead of just "assertion failed".
fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "expected success, got {:?}; stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Best-effort graceful shutdown, called at the end of a test's own success
/// path.
///
/// Mirrors `shep-daemon`'s own `daemon_e2e.rs` `Fixture::shutdown`
/// precedent: on every success path this makes the trailing `DaemonGuard`
/// Drop a no-op, and `DaemonGuard` only does real work on a panic path this
/// function never reaches. It matters for more than tidiness here —
/// `DaemonGuard` SIGKILLs only the daemon's own process group (see its own
/// doc), never the sheep it spawned, while `shep kill` drives the daemon's
/// own graceful stop of each running sheep. Verified empirically with
/// `ps`/`kill` against a real daemon: three back-to-back runs of this suite
/// before this helper existed left eight orphaned `sleep` processes behind,
/// one per sheep started; after adding this call at the end of every case
/// that does not already `kill` as its own subject (plus making every
/// script `exec` into its final `sleep` — see [`write_test_script`]'s own
/// doc for why that half matters too), repeated runs left none.
fn graceful_kill(home: &Path) {
    let _ = shep(home).arg("kill").output();
}

/// A `$SHEP_HOME` whose daemon this test spawned, reaped on `Drop` even if
/// the test panics before its own assertions run.
///
/// # A gap this guard does not close
///
/// This reaps the *daemon* — the process this crate's own launcher
/// (`launch.rs`) makes its own process-group leader via
/// `Command::process_group(0)`, so `-pid` reaches it and nothing else.
/// `shep-daemon`'s `tokio_runner.rs` gives every *sheep* the exact same
/// treatment, deliberately, so the daemon's own `kill_tree` can target one
/// sheep without also hitting itself
/// (`crates/shep-daemon/src/tokio_runner.rs:153-156`) — which means a sheep
/// is never in the daemon's process group either, and SIGKILLing the daemon
/// does not reach it. A sheep orphaned by a panicking test therefore keeps
/// running, reparented, until its own script exits — which is exactly why
/// every script this file writes sleeps for [`SCRIPT_SLEEP_SECS`] and not
/// longer: closing this gap for real would mean every case tracking its own
/// sheep pids the way `shep-daemon`'s own `daemon_e2e.rs` fixture does,
/// which this tier has no RPC-free way to learn without widening scope
/// beyond what Task 12 asks for.
#[derive(Debug, Default)]
struct DaemonGuard(Vec<PathBuf>);

impl DaemonGuard {
    /// Register a `$SHEP_HOME` whose daemon this test is responsible for.
    /// Call it on the `Output` — that is, immediately after `.output()` and
    /// BEFORE the assertion on `output.status`, which panics on failure.
    /// Registering after the assertion leaks exactly the daemon the guard
    /// exists to reap, in exactly the case (a failed autostart) where a
    /// leaked daemon is most likely.
    fn adopt_home(&mut self, home: &Path) {
        self.0.push(home.to_path_buf());
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        for home in &self.0 {
            let Ok(text) = std::fs::read_to_string(home.join("pids/shepd.pid")) else {
                continue;
            };
            let Ok(pid) = text.trim().parse::<i32>() else {
                continue;
            };
            let pid = nix::unistd::Pid::from_raw(pid);
            // Group, not leader: the daemon's own children are in its group.
            // But only while the daemon really IS its own group leader —
            // signalling `-pid` when it is not reaches somebody else's group,
            // and in a test runner that group contains the harness. Case 1
            // asserts the leader property holds; this checks it rather than
            // assuming it, because Drop also runs on the path where case 1
            // failed. ESRCH here means already reaped: fall back to the
            // leader-only signal, which is a no-op in that case.
            let target = match nix::unistd::getpgid(Some(pid)) {
                Ok(pgid) if pgid == pid => nix::unistd::Pid::from_raw(-pid.as_raw()),
                _ => pid,
            };
            // ESRCH on an already-reaped daemon is the expected happy path.
            let _ = nix::sys::signal::kill(target, nix::sys::signal::Signal::SIGKILL);
        }
    }
}

/// Reads the daemon pid recorded at `home`'s pidfile — the same path
/// `shep_daemon::boot::pidfile` builds.
fn read_daemon_pid(home: &Path) -> nix::unistd::Pid {
    let path = home.join("pids/shepd.pid");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("no pidfile at {}: {e}", path.display()));
    let raw: i32 = text
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("bad pidfile contents {text:?}: {e}"));
    nix::unistd::Pid::from_raw(raw)
}

/// Asserts `pid` is the leader of its own process group — the
/// `Command::process_group(0)` contract `launch.rs` relies on to detach the
/// daemon from the parent's group and terminal. `std::process::Command`
/// exposes no getter for this, so a real spawn is the only honest check.
fn assert_group_leader(pid: nix::unistd::Pid) {
    assert_eq!(
        nix::unistd::getpgid(Some(pid)).unwrap(),
        pid,
        "the daemon must be its own process-group leader"
    );
}

/// Runs `shep --home <home> bleats --no-follow` with `args` appended, until
/// its stdout is non-empty or [`BLEATS_DEADLINE`] expires, and returns the
/// last attempt's `Output` either way. The selector and any global flag
/// ride in `args` — `--format` is declared `global = true`
/// (`crates/shep-cli/src/cli.rs`), so clap takes it after the subcommand.
///
/// The retry covers a real gap: `shep start` returns once the sheep is
/// registered and spawned, while the daemon's log pump is a separate task
/// that has not necessarily written the child's first line yet. Polling the
/// log file at its conventional path instead would tie this tier to a
/// path-derivation rule the daemon is free to change (and which an app's
/// own `out_file` overrides anyway); polling the command does not.
///
/// It returns on expiry rather than panicking, so the failure that reaches
/// CI is the caller's own assertion naming its own marker. Each attempt
/// still carries the same [`CMD_TIMEOUT`] every other case does, so nothing
/// here can block unbounded.
fn bleats_no_follow_until_written(home: &Path, args: &[&str]) -> Output {
    let start = Instant::now();
    loop {
        let output = shep(home)
            .arg("bleats")
            .arg("--no-follow")
            .args(args)
            .output()
            .unwrap();
        if !output.stdout.is_empty() || start.elapsed() >= BLEATS_DEADLINE {
            return output;
        }
        std::thread::sleep(BLEATS_POLL_INTERVAL);
    }
}

// --- JSON fixture helpers -------------------------------------------------

/// Asserts `info` (one `data[]` element of a `flock`/`describe`/`start`
/// envelope, or the `describe`d sheep itself) carries the dynamic fields a
/// real spawned sheep must have, then blanks them to `null` so the rest of
/// the object can be compared against a committed fixture verbatim.
///
/// A real `pid` and `uptime_ms` cannot be pinned across runs, and
/// `out_file`/`err_file` are rooted under this test's own tempdir, which
/// differs on every run — the fixture would have to be rewritten by every
/// test invocation to stay byte-exact. Blanking them is not a licence to
/// skip checking them: each is asserted against its own real shape first.
fn normalize_process_info(info: &mut serde_json::Value, home: &Path, name: &str) {
    let pid = info["pid"]
        .as_i64()
        .unwrap_or_else(|| panic!("pid must be a real positive OS pid: {info}"));
    assert!(pid > 0, "pid must be a real positive OS pid: {info}");
    info["uptime_ms"]
        .as_u64()
        .unwrap_or_else(|| panic!("uptime_ms must be present: {info}"));
    let home_str = home.to_str().unwrap();
    for (key, stream) in [("out_file", "out"), ("err_file", "err")] {
        let path = info[key]
            .as_str()
            .unwrap_or_else(|| panic!("{key} must be a string: {info}"));
        assert!(
            path.starts_with(home_str),
            "{key} must be rooted under $SHEP_HOME: {path}"
        );
        assert!(
            path.ends_with(&format!("{name}-0-{stream}.log")),
            "{key} must name this sheep's own log file: {path}"
        );
    }
    info["pid"] = serde_json::Value::Null;
    info["uptime_ms"] = serde_json::Value::Null;
    info["out_file"] = serde_json::Value::Null;
    info["err_file"] = serde_json::Value::Null;
}

/// Parses `output.stdout` as a `flock`/`describe`/`start` envelope,
/// normalizes its one `data[]` element (this whole file only ever starts
/// one sheep per `$SHEP_HOME` in these cases), and compares the result
/// against the committed fixture named `command`.
fn assert_envelope_matches_fixture(output: &Output, home: &Path, command: &str, sheep_name: &str) {
    let mut envelope: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
            panic!(
                "{command}: stdout was not JSON: {e}: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        });
    {
        let data = envelope["data"]
            .as_array()
            .unwrap_or_else(|| panic!("{command}: data must be an array"));
        assert_eq!(data.len(), 1, "{command}: exactly one sheep is expected");
    }
    normalize_process_info(&mut envelope["data"][0], home, sheep_name);
    assert_eq!(
        envelope,
        load_fixture(command),
        "{command} envelope drifted from its committed fixture"
    );
}

/// Asserts a failed `--format json` invocation kept `stdout` empty and put
/// a parseable `{"schema_version", "error": {"code", "message"}}` object on
/// `stderr` — the two-real-streams claim only this tier can prove
/// (`output::emit_error` is handed one writer in a unit test).
fn assert_json_error(output: &Output, expected_status: i32, expected_error_code: &str) {
    assert_eq!(
        output.status.code(),
        Some(expected_status),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty on failure: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let err: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap_or_else(|e| {
        panic!(
            "stderr was not JSON: {e}: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert_eq!(err["error"]["code"], expected_error_code, "{err}");
}

// --- Case 1 ----------------------------------------------------------------

/// `shep start <script>` with no daemon running autostarts one, the sheep
/// reaches Online, and the daemon is its own process-group leader.
///
/// The leader assertion is the `Command::process_group(0)` contract
/// `launch.rs` relies on; `std::process::Command` exposes no getter for it,
/// so a real spawn (which this case already has) is the only honest test.
///
/// What a broken implementation this would catch: a `launch_command` that
/// dropped `.process_group(0)` (the daemon inherits the test harness's own
/// group, and `assert_group_leader` fails); a `main.rs` dispatch that
/// routed `Start` through `connect_client` instead of
/// `connect_or_spawn_client` (nothing would ever be listening, and the
/// command would time out or exit `DaemonUnreachable` instead of
/// `Success`); a supervisor that left a freshly spawned sheep `Starting`
/// rather than `Online` (the JSON assertion below fails by itself).
#[test]
fn starting_with_no_daemon_running_autostarts_one_and_the_sheep_reaches_online() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();

    let output = shep(dir.path())
        .arg("--format")
        .arg("json")
        .arg("start")
        .arg(&script)
        .output()
        .unwrap();
    guard.adopt_home(dir.path());
    assert_success(&output);

    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["data"][0]["status"], "online", "{envelope}");

    let pid = read_daemon_pid(dir.path());
    assert_group_leader(pid);

    graceful_kill(dir.path());
}

// --- Case 2 ------------------------------------------------------------

/// A second command reuses the daemon rather than spawning a second daemon.
///
/// What a broken implementation this would catch: a `connect_or_spawn`
/// probe that always launched (never actually tried connecting first) —
/// the second `start` would produce a distinct pid, failing the equality
/// assertion; a daemon that did not persist registered sheep across
/// requests on the same connection lineage — the final `flock` would be
/// missing `alpha`.
#[test]
fn a_second_command_reuses_the_daemon_rather_than_spawning_a_second() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();

    let first = shep(home)
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("alpha")
        .output()
        .unwrap();
    guard.adopt_home(home);
    assert_success(&first);
    let first_pid = read_daemon_pid(home);

    let second = shep(home)
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("beta")
        .output()
        .unwrap();
    assert_success(&second);
    let second_pid = read_daemon_pid(home);

    assert_eq!(
        first_pid, second_pid,
        "the second command must reuse the first daemon, not spawn a new one"
    );

    let flock = shep(home)
        .arg("--format")
        .arg("json")
        .arg("flock")
        .output()
        .unwrap();
    assert_success(&flock);
    let envelope: serde_json::Value = serde_json::from_slice(&flock.stdout).unwrap();
    let names: Vec<&str> = envelope["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["alpha", "beta"],
        "both sheep must be registered against the one daemon: {envelope}"
    );

    graceful_kill(home);
}

// --- Case 3 --------------------------------------------------------------

/// Two concurrent `shep start` invocations against a cold `$SHEP_HOME`
/// produce exactly one daemon and no spurious error: this is the race
/// Phase 2b's `flock(2)` makes safe daemon-side, and `connect_or_spawn`
/// (`shep-client/src/spawn.rs`) safe client-side — the loser's launched
/// child exits carrying `DAEMON_ALREADY_RUNNING`, `connect_or_spawn` keeps
/// probing instead of surfacing that as an error, and both invocations
/// exit 0.
///
/// A `std::sync::Barrier` synchronizes the two racer threads' `.output()`
/// calls (the actual `Command::spawn()`), matching the synchronization
/// `shep-daemon`'s own `two_concurrent_boots_on_a_stale_socket_exactly_one_wins`
/// uses for the same reason: without it, OS scheduling could let one racer
/// finish entirely before the other starts, which would still pass this
/// test's assertions but would not actually be racing anything.
///
/// This test does not (and, from outside two black-box processes, cannot)
/// observe the losing daemon's own exit status directly — that status is
/// read internally by the `shep start` process that launched it, never
/// exposed across the process boundary. What it asserts instead is the
/// externally observable consequence the brief itself describes: both
/// invocations succeed, and afterward there is exactly one live,
/// group-leader daemon that both racers' sheep are registered against.
///
/// What a broken implementation this would catch: a client-side race in
/// `connect_or_spawn` that treated `DAEMON_ALREADY_RUNNING` as a fatal
/// error instead of "keep probing" — the losing invocation would exit
/// non-zero, failing `assert_success`; two daemons somehow both surviving —
/// `flock` would show one racer's sheep missing (whichever daemon that
/// query happened to reach would only know about its own).
#[test]
fn concurrent_cold_starts_produce_exactly_one_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();
    // Registered before racing at all, not on an `Output` — two racers
    // means two Outputs and no single point that precedes every panic path,
    // so the earliest safe point is before either thread starts.
    guard.adopt_home(&home);

    let names = ["racer-a", "racer-b"];
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(names.len()));
    let handles: Vec<_> = names
        .iter()
        .map(|name| {
            let home = home.clone();
            let script = script.clone();
            let name = (*name).to_string();
            let barrier = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait(); // both racers launch together
                shep(&home)
                    .arg("start")
                    .arg(&script)
                    .arg("--name")
                    .arg(&name)
                    .output()
                    .unwrap()
            })
        })
        .collect();

    let outputs: Vec<Output> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    for (name, output) in names.iter().zip(&outputs) {
        assert!(
            output.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let pid = read_daemon_pid(&home);
    assert_group_leader(pid);

    let flock = shep(&home)
        .arg("--format")
        .arg("json")
        .arg("flock")
        .output()
        .unwrap();
    assert_success(&flock);
    let envelope: serde_json::Value = serde_json::from_slice(&flock.stdout).unwrap();
    let mut got: Vec<&str> = envelope["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    got.sort_unstable();
    assert_eq!(
        got,
        ["racer-a", "racer-b"],
        "both racers must have registered against the SAME daemon: {envelope}"
    );

    graceful_kill(&home);
}

// --- Case 4 ----------------------------------------------------------------

/// `--format json` output validates against the committed fixture for
/// `flock`, `describe`, `start` and `ping` (envelopes, compared structurally
/// after normalizing the fields a real spawned sheep cannot pin across
/// runs), and for `bleats --no-follow` (one JSON object, no envelope,
/// compared byte-for-byte).
///
/// One `$SHEP_HOME`, one sheep ("fixture", id 0 — a fresh `$SHEP_HOME`
/// allocates ids from zero, `shep-daemon/src/supervisor.rs:299`) shared by
/// all five checks: `start`'s own response, then `flock`/`describe`/`ping`
/// against the same running daemon, then the bleats fixture — the sheep's
/// single stdout marker is what keeps `bleats --no-follow`'s stdout to
/// exactly the one line the fixture pins.
///
/// What a broken implementation this would catch, per surface: a renamed,
/// reordered, or dropped `ProcessInfo`/`BleatLine`/`PingRow` field (the
/// structural or byte-exact comparison fails); an `id` allocator that did
/// not start from zero on a fresh home; a `command` string mismatched to
/// its own verb (`describe`/`fold` sharing one code path is exactly the
/// class of bug Task 9's own reviewer found — an envelope with `describe`
/// and `flock` swapped would fail exactly one of these four checks); a
/// `bleats --no-follow` that produced empty output (the byte-exact
/// comparison against a non-empty fixture fails, unlike a mere
/// `.is_empty()` check, which an empty stdout would also pass).
#[test]
fn json_format_matches_the_committed_fixtures() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script = write_logging_script(&dir, "fixture-line-1", None);
    let mut guard = DaemonGuard::default();

    let start_out = shep(home)
        .arg("--format")
        .arg("json")
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("fixture")
        .output()
        .unwrap();
    guard.adopt_home(home);
    assert_success(&start_out);
    assert_envelope_matches_fixture(&start_out, home, "start", "fixture");

    let flock_out = shep(home)
        .arg("--format")
        .arg("json")
        .arg("flock")
        .output()
        .unwrap();
    assert_success(&flock_out);
    assert_envelope_matches_fixture(&flock_out, home, "flock", "fixture");

    let describe_out = shep(home)
        .arg("--format")
        .arg("json")
        .arg("describe")
        .arg("fixture")
        .output()
        .unwrap();
    assert_success(&describe_out);
    assert_envelope_matches_fixture(&describe_out, home, "describe", "fixture");

    let ping_out = shep(home)
        .arg("--format")
        .arg("json")
        .arg("ping")
        .output()
        .unwrap();
    assert_success(&ping_out);
    let mut ping_envelope: serde_json::Value = serde_json::from_slice(&ping_out.stdout).unwrap();
    let ping_pid = ping_envelope["data"]["pid"]
        .as_i64()
        .unwrap_or_else(|| panic!("ping must report a real pid: {ping_envelope}"));
    assert!(ping_pid > 0);
    assert_eq!(
        nix::unistd::Pid::from_raw(i32::try_from(ping_pid).unwrap()),
        read_daemon_pid(home),
        "ping's pid must be the daemon's own pid"
    );
    ping_envelope["data"]["pid"] = serde_json::Value::Null;
    assert_eq!(
        ping_envelope,
        load_fixture("ping"),
        "ping envelope drifted from its committed fixture"
    );

    let bleats_out = bleats_no_follow_until_written(home, &["all", "--format", "json"]);
    assert_eq!(
        bleats_out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&bleats_out.stderr)
    );
    let expected = std::fs::read(fixture_path("bleats_no_follow")).unwrap();
    assert_eq!(
        bleats_out.stdout,
        expected,
        "bleats --no-follow --format json must match its fixture byte-for-byte: got {}",
        String::from_utf8_lossy(&bleats_out.stdout)
    );

    graceful_kill(home);
}

// --- Case 5 ------------------------------------------------------------

/// Exit codes and stream discipline, under `--format json`: a selector
/// matching nothing exits `NotFound`; the malformed selector `/[/` exits
/// `Usage` (`/unclosed` would not — it parses as a sheep literally named
/// `/unclosed` and would exit `NotFound`, testing nothing); a nonexistent
/// `--home` on a non-autostarting verb (`flock`) exits `DaemonUnreachable`.
/// For each, stdout must stay empty and stderr must parse as a JSON object
/// carrying `error.code`.
///
/// The first two need a live daemon: every non-`Start` verb connects
/// *before* parsing its own selector (`main.rs`'s dispatch), so both
/// failure modes only reach their own code path once a daemon has already
/// answered — a cold `$SHEP_HOME` here would exit `DaemonUnreachable`
/// before ever reaching the selector at all, hiding both.
///
/// What a broken implementation this would catch: `describe` skipping its
/// client-side `ProcessSelector::parse` and shipping `/[/` to the daemon
/// (it would come back `NotFound`, not `Usage`, and this is exactly the
/// bug Task 8 documented three rejected-selector-string traps for); `start`
/// substituted for `flock` in the third sub-case (`start` autostarts and
/// would *create* the nonexistent directory, exiting `Success`); an
/// `emit_error` call that wrote to `stdout` instead of `stderr`, or that
/// wrote table-mode prose regardless of `--format` — both fail the
/// structural stderr-is-JSON assertion in a way a `.contains(...)` check on
/// combined output could not.
#[test]
fn exit_codes_and_stream_discipline() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();

    let boot = shep(home)
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("only")
        .output()
        .unwrap();
    guard.adopt_home(home);
    assert_success(&boot);

    let not_found = shep(home)
        .arg("--format")
        .arg("json")
        .arg("describe")
        .arg("ghost")
        .output()
        .unwrap();
    assert_json_error(&not_found, 3, "not_found");

    let usage = shep(home)
        .arg("--format")
        .arg("json")
        .arg("describe")
        .arg("/[/")
        .output()
        .unwrap();
    assert_json_error(&usage, 2, "usage");

    // A separate, never-created home: `flock` never autostarts (only
    // `start` does, per `main.rs`), so "nonexistent directory" stays true
    // for the whole invocation, unlike `start` against the same path.
    let cold = tempfile::tempdir().unwrap();
    let missing_home = cold.path().join("gone");
    let unreachable = shep(&missing_home)
        .arg("--format")
        .arg("json")
        .arg("flock")
        .output()
        .unwrap();
    assert_json_error(&unreachable, 5, "daemon_unreachable");
    // `missing_home` never had a daemon (that is the point of this
    // sub-case) — nothing to gracefully kill there. `home` does.

    graceful_kill(home);
}

// --- Case 6 --------------------------------------------------------------

/// `shep kill` stops the daemon and removes the socket.
///
/// What a broken implementation this would catch: a `kill` that reported
/// success straight off `Response::ShuttingDown` without polling for the
/// socket to actually disappear (`commands/admin.rs`'s own documented
/// race) — on a slow teardown this would pass regardless, but a `kill` that
/// never sent `Request::KillDaemon` at all, or that the daemon never wired
/// to its own teardown, leaves the socket behind and fails the final
/// assertion outright.
#[test]
fn kill_stops_the_daemon_and_removes_the_socket() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();

    let boot = shep(home).arg("start").arg(&script).output().unwrap();
    guard.adopt_home(home);
    assert_success(&boot);

    let socket = home.join("run/shep.sock");
    assert!(socket.exists(), "precondition: the daemon is up");

    let kill = shep(home).arg("kill").output().unwrap();
    assert_success(&kill);
    assert!(!socket.exists(), "kill must remove the socket file");
}

// --- Case 7 --------------------------------------------------------------

/// `shep bleats --no-follow` prints what a sheep actually wrote to its log
/// files: both of a sheep's streams by default, and only the requested one
/// under `--out`.
///
/// What a broken implementation this would catch: a `--no-follow` that
/// printed nothing at all (the old bus-backed drain arm this case was
/// blocked on until Task 10a) — it would pass an `exits Success` check but
/// fail the stdout-contains assertions here; a `--out`/`--err` that was
/// accepted and ignored rather than actually selecting a file — the second
/// half's negative assertion (`bleater-err-marker` absent) is what only a
/// real file-selecting implementation can pass.
#[test]
fn bleats_no_follow_prints_what_a_sheep_actually_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script = write_logging_script(&dir, "bleater-out-marker", Some("bleater-err-marker"));
    let mut guard = DaemonGuard::default();

    let boot = shep(home)
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("bleater")
        .output()
        .unwrap();
    guard.adopt_home(home);
    assert_success(&boot);

    let both = bleats_no_follow_until_written(home, &["all"]);
    assert_eq!(
        both.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&both.stderr)
    );
    let stdout = String::from_utf8_lossy(&both.stdout);
    let stderr = String::from_utf8_lossy(&both.stderr);
    assert!(stdout.contains("bleater-out-marker"), "stdout={stdout}");
    assert!(stdout.contains("bleater-err-marker"), "stdout={stdout}");
    assert!(
        !stderr.contains("bleater-out-marker") && !stderr.contains("bleater-err-marker"),
        "a sheep's own lines must never reach shep's diagnostic stream: stderr={stderr}"
    );

    let out_only = bleats_no_follow_until_written(home, &["all", "--out"]);
    assert_eq!(
        out_only.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out_only.stderr)
    );
    let stdout_only = String::from_utf8_lossy(&out_only.stdout);
    assert!(
        stdout_only.contains("bleater-out-marker"),
        "stdout={stdout_only}"
    );
    assert!(
        !stdout_only.contains("bleater-err-marker"),
        "--out must select the out file only: stdout={stdout_only}"
    );

    graceful_kill(home);
}

// --- Case 8 --------------------------------------------------------------

/// `shep --home <tmp> start <script>` autostarts a daemon whose socket is
/// under `<tmp>` — asserted on the location of the socket file, not on the
/// command exiting 0, so a child that re-resolved `$SHEP_HOME` from ambient
/// environment (rather than the `SHEP_HOME` `launch.rs` explicitly sets on
/// the re-exec'd child) and bound elsewhere still fails this even though it
/// would exit `Success`.
///
/// `env_remove("SHEP_HOME")` is the point: without it, an ambient
/// `$SHEP_HOME` the test process happened to inherit could make this pass
/// for the wrong reason.
///
/// Code below is the brief's own given form, verbatim — including relying
/// on `DaemonGuard` alone rather than this file's own `graceful_kill`
/// helper, so this one case's sheep is left for `DaemonGuard`'s SIGKILL to
/// orphan (see that type's own doc) rather than the daemon's real kill
/// ladder to reap. Bounded by `SCRIPT_SLEEP_SECS`, same as every other case.
#[test]
fn home_reaches_the_spawned_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();

    let output = Command::cargo_bin("shep")
        .unwrap()
        .args([
            "--home",
            dir.path().to_str().unwrap(),
            "start",
            script.to_str().unwrap(),
        ])
        .env_remove("SHEP_HOME") // the ambient value must not be what makes this pass
        .timeout(Duration::from_secs(30)) // never block unbounded; see above
        .output()
        .unwrap();

    // Registered on the Output, before anything that can panic — a failed
    // autostart is precisely when a daemon is most likely to be left behind.
    guard.adopt_home(dir.path());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let socket = dir.path().join("run/shep.sock");
    assert!(
        socket.exists(),
        "the daemon bound somewhere other than --home"
    );
}
