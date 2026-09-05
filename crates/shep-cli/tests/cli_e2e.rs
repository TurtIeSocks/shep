//! End-to-end tier: drives the real `shep` binary via `assert_cmd` against a
//! real daemon, a real socket, and real spawned sheep, each on a fresh
//! `$SHEP_HOME` in its own [`tempfile::TempDir`].
//!
//! Two rules every case follows: `.timeout(CMD_TIMEOUT)` before `.output()`,
//! so a hang fails as a named assertion; and a [`DaemonGuard`] adopting the
//! `$SHEP_HOME` immediately after the `Output` that might have spawned a
//! daemon, before any assertion that could panic.
//!
//! Windows scripts are `.cmd` (see `script_header`); cases that cannot port
//! carry their own `#[cfg(unix)]`.

// The `#[cfg(unix)]` cases take their helpers and constants with them, so on
// Windows those items compile unused.
#![cfg_attr(windows, allow(dead_code))]

#[cfg(unix)]
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use assert_cmd::cargo::CommandCargoExt as _;
use tempfile::TempDir;

/// Bound on every `shep` invocation here; `.output()` blocks unbounded
/// without it.
///
/// Must outlive [`shep_client::spawn::SPAWN_DEADLINE`]: the autostart path can
/// spend that whole budget before reporting `DaemonUnreachable`, and an equal
/// bound kills the process before it can report exit 5.
const CMD_TIMEOUT: Duration =
    Duration::from_secs(shep_client::spawn::SPAWN_DEADLINE.as_secs() + 15);

/// Bound on how long [`concurrent_cold_starts_produce_exactly_one_daemon`]
/// waits for one of its racers.
///
/// [`CMD_TIMEOUT`] bounds the process wait, not the reader threads after it:
/// those end on EOF, which waits for every copy of the write end, including
/// one a daemon inherited. Strictly longer than [`CMD_TIMEOUT`], so it fires
/// only on a stuck racer.
const RACER_DEADLINE: Duration = Duration::from_secs(CMD_TIMEOUT.as_secs() + 15);

/// How long [`bleats_no_follow_until_written`] keeps retrying.
const BLEATS_DEADLINE: Duration = Duration::from_secs(10);

/// Gap between [`bleats_no_follow_until_written`]'s retries.
const BLEATS_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long a fixture sheep's script sleeps after writing whatever it writes.
///
/// Outlasts every case that uses it, and short enough that a sheep the
/// [`DaemonGuard`] sweep missed self-terminates. The real-clock cases use
/// [`SLOW_SCRIPT_SLEEP_SECS`].
const SCRIPT_SLEEP_SECS: u32 = 60;

/// [`SCRIPT_SLEEP_SECS`] for the two real-clock cases.
///
/// Twice [`CRON_DEADLINE`], the longest of their deadlines: a script that
/// could exit inside the observation window would make "the sheep restarted"
/// equally consistent with a crash loop.
const SLOW_SCRIPT_SLEEP_SECS: u32 = 300;

/// Basename, under a case's own `$SHEP_HOME`, of the file every fixture
/// script appends its own pid to. Written by [`record_pid_line`], read by
/// [`DaemonGuard`].
const FIXTURE_PIDS: &str = "fixture.pids";

/// How long [`DaemonGuard::drop`] keeps retrying for a parseable daemon pid.
///
/// `PidfileLock::acquire` creates the pidfile empty and `record` fills it only
/// once the control socket is bound, so a fresh `$SHEP_HOME` has an empty
/// pidfile for the whole bind.
const GUARD_PID_DEADLINE: Duration = Duration::from_secs(3);

/// Gap between [`GUARD_PID_DEADLINE`]'s and [`GUARD_SWEEP_WINDOW`]'s retries.
const GUARD_PID_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long [`sweep_flock`] keeps re-reading a case's recorded sheep pids.
/// Covers the gap between the spawn `shep start` reports as `Online` and the
/// script's first line, which is when the pid reaches disk.
const GUARD_SWEEP_WINDOW: Duration = Duration::from_secs(2);

/// How long [`poll_flock`] keeps asking before returning what it last saw.
///
/// One deadline for both directions: a case waiting for a restart and a case
/// proving none came must wait the same length. Sized against the 500ms
/// `DEFAULT_WATCH_DELAY` debounce plus a spawn and two RPC round trips, with
/// an order of magnitude of headroom.
const FLOCK_DEADLINE: Duration = Duration::from_secs(10);

/// Gap between [`poll_flock`]'s attempts.
const FLOCK_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// `RequestError::Closed`: the reply a client loses when the image on the
/// other end of its request was replaced by a handover. An accepted
/// connection is `FD_CLOEXEC` and dies at the `execve`; the handover spec's
/// H2 table rules that the client sees the drop.
const DROPPED_REPLY: &str = "the connection closed before a reply arrived";

/// `ConnectError::HandshakeClosed`: the same exec, caught between the accept
/// and the `HelloReply`. A shepherd that is gone prints "could not connect"
/// instead, so neither is reachable from a dead one.
const DROPPED_HANDSHAKE: &str = "the daemon closed the connection during the handshake";

/// How long [`poll_metrics`] keeps retrying a `/metrics` scrape.
///
/// `shep enable metrics` returns once the `EnableDog` RPC is accepted, before
/// the daemon has exec'd `shep dog metrics` or that process has bound.
const METRICS_SCRAPE_DEADLINE: Duration = FLOCK_DEADLINE;

/// Gap between [`poll_metrics`]'s retries.
const METRICS_SCRAPE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Bound on a single scrape attempt's own I/O, inside [`poll_metrics`]'s retry
/// loop: a peer that connects and then never answers must not stall it past
/// its own deadline.
const METRICS_SCRAPE_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// How long [`poll_http_get`] keeps retrying a `shep serve` worker. `shep
/// serve` returning success means the sheep is registered, not that the worker
/// has bound its listener.
const SERVE_HTTP_DEADLINE: Duration = FLOCK_DEADLINE;

/// Gap between [`poll_http_get`]'s retries.
const SERVE_HTTP_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Bound on a single `shep serve` request's own I/O, inside
/// [`poll_http_get`]'s retry loop.
const SERVE_HTTP_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Bound [`a_served_sheep_stops_on_sigterm_rather_than_riding_the_ladder_to_sigkill`]
/// asserts `shep stop`'s wall clock against.
///
/// `Command::Stop` defers its reply until the sheep has exited, so elapsed
/// time reports which rung of the kill ladder answered: `SIGKILL` takes at
/// least `kill_timeout`, 1600ms; a handled `SIGTERM` takes tens of
/// milliseconds.
const SERVE_STOP_DEADLINE: Duration = Duration::from_millis(1000);

/// How long [`a_cron_occurrence_restarts_a_sheep_on_the_real_clock`] waits.
///
/// A `* * * * *` pattern armed at an arbitrary moment is up to 60s from its
/// first occurrence. Two and a half minutes covers two, so a loaded runner
/// that misses the first still has a second. The case costs 26s to 61s.
const CRON_DEADLINE: Duration = Duration::from_secs(150);

/// How long [`a_real_memory_breach_restarts_a_sheep`] waits for its breach.
///
/// The enforcer samples every `shep_daemon::limits::MEMORY_POLL_INTERVAL`
/// (15s), phased off daemon boot, so the worst wait is one whole interval plus
/// a kill ladder and a respawn. Four times that is headroom.
const BREACH_DEADLINE: Duration = Duration::from_secs(60);

/// How long a string [`write_ballooning_script`] grows, in bytes.
///
/// Growing a 16 MiB string takes a `/bin/sh` from about 1.2 MB resident to
/// about 166 MB: the doubling loop's intermediate allocations stay in its
/// heap. The string alone is twice [`BREACH_LIMIT`].
const BALLOON_BYTES: u64 = 16 * 1024 * 1024;

/// The `max_memory` the ballooning sheep is given: above a bare shell's 1.2 MB
/// resident set and half the string it grows, so it is under the ceiling
/// before and over it after.
const BREACH_LIMIT: &str = "8M";

/// The `listen_timeout` [`write_never_ready_flockfile`] gives its sheep.
///
/// Nothing races it: the sheep never signals, so this is a delay before a
/// certainty. The daemon takes a timed-out `wait_ready` sheep `Online`, so the
/// elapse shows in `shep flock` too.
const NEVER_READY_TIMEOUT: &str = "1s";

/// What [`write_rotating_script`]'s sheep prints before the rotation, and
/// what must end up in the renamed archive rather than in the recreated log.
const ROTATE_BEFORE: &str = "before-the-rotation";

/// What the same sheep prints after it. Its arrival in the recreated file is
/// the whole assertion.
const ROTATE_AFTER: &str = "after-the-rotation";

/// The daemon record the two log-plane cases read out of
/// `$SHEP_HOME/logs/shepd.err.log`, written at `WARN` by
/// `Actor::handle_ready_result`.
///
/// One owner for the string: the two cases assert opposite things about the
/// same record, and a drifted pair would keep passing while proving nothing.
const READINESS_RECORD: &str = "readiness deadline elapsed";

// --- Fixture helpers ---------------------------------------------------

/// The path of the committed `--format json` fixture named `name`.
fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(format!("{name}.json"))
}

/// Loads and parses a committed fixture, for the envelope fixtures compared
/// structurally as a `serde_json::Value`. `bleats_no_follow.json` is compared
/// byte for byte through `std::fs::read` instead.
fn load_fixture(name: &str) -> serde_json::Value {
    let path = fixture_path(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Writes a trivial long-running script into `dir` and returns its path.
///
/// The trailing `sleep` is bare, not `exec sleep`: a bare one is a forked
/// child of the `/bin/sh` the daemon tracks, sharing its process group, which
/// is what a stop signalling only the recorded pid would orphan.
fn write_test_script(dir: &TempDir) -> PathBuf {
    write_script(
        dir,
        "sheep.sh",
        &format!(
            "{}{}{}",
            script_header(),
            record_pid_line(dir),
            sleep_line(SCRIPT_SLEEP_SECS)
        ),
    )
}

/// Writes a script that backgrounds a `sleep 300` and `wait`s on it, a real
/// forked lamb for [`describe_renders_a_real_sheeps_lamb_tree`].
///
/// `wait` keeps the top-level `sh` alive as long as its child, so the daemon's
/// pid stays the one this test started and a stop still reaches the lamb
/// through the shared process group.
fn write_forking_script(dir: &TempDir) -> PathBuf {
    write_script(
        dir,
        "forker.sh",
        &format!("#!/bin/sh\n{}sleep 300 &\nwait\n", record_pid_line(dir)),
    )
}

/// The line every fixture script opens with: this spawn's own pid, appended
/// to `<home>/`[`FIXTURE_PIDS`].
///
/// `$$` is the pid the daemon tracks and leads its own process group, so
/// `-pid` reaches that sheep's lambs and [`DaemonGuard`]'s sweep can reap a
/// whole flock. Appended, so a restart adds a row; the dead pid is an `ESRCH`
/// no-op later.
///
/// The path is absolute, since a script's cwd is the sheep's `cwd`, and
/// quoted, since a tempdir path may carry shell metacharacters. It never goes
/// to stdout: one extra line breaks the byte-exact `bleats` fixture.
fn record_pid_line(dir: &TempDir) -> String {
    #[cfg(unix)]
    {
        format!(
            "echo $$ >> \"{}\"\n",
            dir.path().join(FIXTURE_PIDS).display()
        )
    }
    // No `$$` in `cmd.exe`, and none needed: a Windows sheep is in a job object
    // it cannot leave, so the daemon dying takes the whole tree with it.
    #[cfg(windows)]
    {
        let _ = dir;
        String::new()
    }
}

/// [`write_test_script`] with [`SLOW_SCRIPT_SLEEP_SECS`]' sleep, for
/// [`a_cron_occurrence_restarts_a_sheep_on_the_real_clock`], which runs one as
/// its subject and one as its control.
fn write_slow_script(dir: &TempDir) -> PathBuf {
    write_script(
        dir,
        "slow.sh",
        &format!(
            "{}{}{}",
            script_header(),
            record_pid_line(dir),
            sleep_line(SLOW_SCRIPT_SLEEP_SECS)
        ),
    )
}

/// Writes a script that grows its own resident set past [`BALLOON_BYTES`] and
/// then sleeps for [`SLOW_SCRIPT_SLEEP_SECS`].
///
/// The growth is a shell string doubled in place, so `$$`, the pid the daemon
/// arms the enforcer against, is the process whose resident set moves. Pure
/// shell arithmetic, so it does not vary with a platform's coreutils, and it
/// costs about a quarter of a second, inside the gap before the enforcer's
/// first tick.
fn write_ballooning_script(dir: &TempDir) -> PathBuf {
    write_script(dir, "balloon.sh", &balloon_body(dir))
}

/// Writes a script that emits one marker line on stdout, optionally one on
/// stderr, and then sleeps.
///
/// `None` writes to stderr not at all: an empty line still reaches the err
/// file and gains the byte-exact fixture an object it did not predict. The
/// sleep keeps the output countable, since a script that exits is restarted
/// and appends another copy of every marker.
fn write_logging_script(dir: &TempDir, out_marker: &str, err_marker: Option<&str>) -> PathBuf {
    let mut script = format!(
        "{}{}{}",
        script_header(),
        record_pid_line(dir),
        echo_line(out_marker)
    );
    if let Some(err_marker) = err_marker {
        script.push_str(&echo_err_line(err_marker));
    }
    script.push_str(&sleep_line(SCRIPT_SLEEP_SECS));
    write_script(dir, "logging.sh", &script)
}

/// [`write_logging_script`] for a multi-instance app: one stdout line naming
/// the slot, read out of the `SHEP_INSTANCE` the daemon injects.
///
/// The slot comes from the child's own environment, not from anything this
/// harness substitutes, since the claim is that the daemon gave each instance
/// a different one. `name` is the script's basename, so several can share one
/// `$TMPDIR`.
fn write_instance_logging_script(dir: &TempDir, name: &str, prefix: &str) -> PathBuf {
    let echo = {
        #[cfg(unix)]
        {
            format!("echo \"{prefix}-$SHEP_INSTANCE\"\n")
        }
        #[cfg(windows)]
        {
            format!("echo {prefix}-%SHEP_INSTANCE%\r\n")
        }
    };
    write_script(
        dir,
        &format!("{name}.sh"),
        &format!(
            "{}{}{}{}",
            script_header(),
            record_pid_line(dir),
            echo,
            sleep_line(SCRIPT_SLEEP_SECS)
        ),
    )
}

/// Writes a script that prints [`ROTATE_BEFORE`], blocks until `gate` exists,
/// prints [`ROTATE_AFTER`], and sleeps.
///
/// A rotation is observable only in what happens to a line written after the
/// rename, and the gate makes "after" a fact rather than a timing bet: the
/// test creates it once the reopen has returned.
fn write_rotating_script(dir: &TempDir, gate: &Path) -> PathBuf {
    write_script(
        dir,
        "rotating.sh",
        &format!(
            "{}{}{}{}{}{}",
            script_header(),
            record_pid_line(dir),
            echo_line(ROTATE_BEFORE),
            wait_for_path_lines(gate),
            echo_line(ROTATE_AFTER),
            sleep_line(SCRIPT_SLEEP_SECS)
        ),
    )
}

/// Writes a script that blocks until `sentinel` exists, then announces
/// readiness on the shepherd channel and sleeps.
///
/// A file, not a delay: `listen_timeout` takes a `wait_ready` sheep `Online`
/// on elapse whether it signalled or not, so a script that merely slept would
/// give a loaded runner a `starting` window it could close early.
///
/// `>&3` is the fd the runner hands a sheep whose app asks for a channel, and
/// `{"kind":"ready"}` is the wire string `ChildMessage::Ready` pins.
fn write_ready_script(dir: &TempDir, sentinel: &Path) -> PathBuf {
    write_script(
        dir,
        "ready.sh",
        &format!(
            "{}{}{}{}{}",
            script_header(),
            record_pid_line(dir),
            wait_for_path_lines(sentinel),
            ready_message_line(),
            sleep_line(SCRIPT_SLEEP_SECS)
        ),
    )
}

/// Shared write-plus-chmod tail of the script helpers above.
fn write_script(dir: &TempDir, name: &str, contents: &str) -> PathBuf {
    let path = dir.path().join(script_name(name));
    std::fs::write(&path, contents).unwrap();
    // Windows has no execute bit: `CreateProcess` decides from the extension,
    // which `script_name` supplied.
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }
    path
}

/// `name` with the extension this platform will actually execute.
///
/// Callers all name their scripts `something.sh`. `CreateProcess` needs an
/// extension `%PATHEXT%` knows, which `.sh` is not and `.cmd` is.
fn script_name(name: &str) -> String {
    #[cfg(unix)]
    {
        name.to_string()
    }
    #[cfg(windows)]
    {
        format!("{}.cmd", name.trim_end_matches(".sh"))
    }
}

/// The first line of a generated script: a `#!` on unix, `@echo off` on
/// Windows so the interpreter does not echo every line into the sheep's own
/// stdout and corrupt what the log-reading cases assert.
fn script_header() -> String {
    #[cfg(unix)]
    {
        "#!/bin/sh\n".to_string()
    }
    #[cfg(windows)]
    {
        "@echo off\r\n".to_string()
    }
}

/// A line that keeps the script alive for roughly `secs` seconds.
///
/// `ping` rather than `timeout` on Windows: `timeout.exe` refuses a
/// non-console stdin, and every sheep gets a null one. `ping -n N` sends N
/// packets a second apart, so it takes one more than the seconds wanted.
fn sleep_line(secs: u32) -> String {
    #[cfg(unix)]
    {
        format!("sleep {secs}\n")
    }
    #[cfg(windows)]
    {
        format!("ping -n {} 127.0.0.1 >nul\r\n", secs + 1)
    }
}

/// A line writing `text` to stdout.
fn echo_line(text: &str) -> String {
    #[cfg(unix)]
    {
        format!("echo '{text}'\n")
    }
    #[cfg(windows)]
    {
        format!("echo {text}\r\n")
    }
}

/// Lines that block until `path` exists, polling.
///
/// `cmd.exe` has no `until` and no sub-second sleep, so the batch arm is an
/// `if exist`/`goto` loop polling with `ping -n 2`.
fn wait_for_path_lines(path: &Path) -> String {
    #[cfg(unix)]
    {
        format!("until [ -e \"{}\" ]; do sleep 0.1; done\n", path.display())
    }
    #[cfg(windows)]
    {
        format!(
            ":wait\r\nif exist \"{}\" goto ready\r\nping -n 2 127.0.0.1 >nul\r\ngoto wait\r\n:ready\r\n",
            path.display()
        )
    }
}

/// A line writing one `ready` shepherd-channel message.
///
/// The channel is inherited fd 3 on unix. Windows has no fd-3 inheritance, so
/// the daemon exports `%SHEP_CHANNEL_PIPE%` and the app opens it by name.
fn ready_message_line() -> String {
    #[cfg(unix)]
    {
        "printf '{\"kind\":\"ready\"}\\n' >&3\n".to_string()
    }
    #[cfg(windows)]
    {
        "echo {\"kind\":\"ready\"}>\"%SHEP_CHANNEL_PIPE%\"\r\n".to_string()
    }
}

/// A line writing `text` to stderr.
fn echo_err_line(text: &str) -> String {
    #[cfg(unix)]
    {
        format!("echo '{text}' 1>&2\n")
    }
    #[cfg(windows)]
    {
        format!("echo {text} 1>&2\r\n")
    }
}

/// The body of [`write_ballooning_script`]: hold [`BALLOON_BYTES`] live, then
/// stay up.
///
/// `cmd.exe` variables cap out around 8 KB, so the Windows arm allocates in
/// PowerShell. shep samples a sheep's whole tree, so a child's memory counts.
fn balloon_body(dir: &TempDir) -> String {
    // Only the unix arm records a pid.
    #[cfg(windows)]
    let _ = dir;
    #[cfg(unix)]
    {
        format!(
            "{}{}s=x\nwhile [ ${{#s}} -lt {BALLOON_BYTES} ]; do s=\"$s$s\"; done\nsleep {SLOW_SCRIPT_SLEEP_SECS}\n",
            script_header(),
            record_pid_line(dir),
        )
    }
    #[cfg(windows)]
    {
        format!(
            "{}powershell -NoProfile -Command \"$s = 'x' * {BALLOON_BYTES}; Start-Sleep -Seconds {SLOW_SCRIPT_SLEEP_SECS}; $s.Length > $null\"\r\n",
            script_header(),
        )
    }
}

/// Writes a Flockfile whose one app asks for a readiness handshake it never
/// performs, so [`NEVER_READY_TIMEOUT`] elapses and the daemon writes
/// [`READINESS_RECORD`] about it.
///
/// A plain [`write_test_script`] sheep is enough: `wait_ready` opens the
/// channel on fd 3 and the script never writes to it.
fn write_never_ready_flockfile(dir: &TempDir) -> PathBuf {
    let script = write_test_script(dir);
    write_flockfile(
        dir,
        &format!(
            "[[app]]\nname = \"gated\"\nscript = '{}'\n\
             wait_ready = true\nlisten_timeout = \"{NEVER_READY_TIMEOUT}\"\n",
            script.display(),
        ),
    )
}

/// Writes `Flockfile.toml` into `dir` and returns its path. The `.toml`
/// extension is what routes `shep start <path>` down `FlockFormat::from_path`
/// rather than the bare-script arm.
fn write_flockfile(dir: &TempDir, body: &str) -> PathBuf {
    let path = dir.path().join("Flockfile.toml");
    std::fs::write(&path, body).unwrap();
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

/// Best-effort graceful shutdown, called at the end of a test's success path.
///
/// [`DaemonGuard`]'s sweep is gated on `std::thread::panicking()`, so on a
/// success path nothing but this reaps the sheep. Without it a run leaks one
/// orphaned `sleep` per sheep started.
fn graceful_kill(home: &Path) {
    let _ = shep(home).arg("kill").output();
}

#[cfg(unix)]
/// Boots a daemon on `dir`'s `$SHEP_HOME` with `env` set on the `shep start`
/// that autostarts it, waits for [`write_never_ready_flockfile`]'s sheep to
/// give up, and hands back the daemon's own log.
///
/// `launch::launch_command` does not `.env_clear()` the re-exec, so `env`
/// reaches the child that installs the subscriber. Waiting for `online` orders
/// the read: `handle_ready_result` writes [`READINESS_RECORD`] before it sets
/// the status. The daemon is killed before the log is returned, so a caller's
/// assertion can panic without leaking a supervisor.
fn daemon_log_after_a_missed_handshake(dir: &TempDir, env: &[(&str, &str)]) -> String {
    let home = dir.path();
    let flockfile = write_never_ready_flockfile(dir);
    let mut guard = DaemonGuard::default();

    let mut start = shep(home);
    for (key, value) in env {
        start.env(key, value);
    }
    let boot = start.arg("start").arg(&flockfile).output().unwrap();
    guard.adopt_home(home);
    assert_success(&boot);

    let online = poll_flock(home, |info| info["status"] == "online");
    assert_eq!(
        online["status"], "online",
        "a wait_ready sheep that never signals must still be taken online once \
         its listen_timeout elapses, which is the record's own trigger: {online}"
    );

    let log = std::fs::read_to_string(home.join("logs").join("shepd.err.log")).unwrap();
    graceful_kill(home);
    log
}

/// A `$SHEP_HOME` whose daemon and whole flock this test is responsible for,
/// reaped on `Drop` even if the test panics first.
///
/// Every sheep has its own process group, so killing the daemon does not reach
/// one; [`record_pid_line`] has each fixture script record its pid so this
/// guard can reach a flock it cannot enumerate over RPC.
///
/// Two orderings are load-bearing. The daemon dies first, or the restart brain
/// brings each killed sheep back. The sweep runs only while panicking: on a
/// success path [`graceful_kill`] has proven these pids gone, and the OS may
/// have recycled them. `Drop` must not panic, so an unreachable daemon is
/// reported with `eprintln!`.
#[derive(Debug, Default)]
struct DaemonGuard {
    homes: Vec<PathBuf>,
    /// Dogs adopted by pid, reaped individually because they are in no home's
    /// flock. Unix only: the Windows arm reaps through `shep kill` and its job
    /// objects.
    #[cfg(unix)]
    dog_pids: Vec<nix::unistd::Pid>,
}

impl DaemonGuard {
    /// Register a `$SHEP_HOME` whose daemon this test is responsible for.
    ///
    /// Call it immediately after `.output()` and before the assertion on
    /// `output.status`: registering after it leaks the daemon in exactly the
    /// failed-autostart case where one is most likely.
    fn adopt_home(&mut self, home: &Path) {
        self.homes.push(home.to_path_buf());
    }

    /// Register a dog's own pid, a grandchild whose process group sits outside
    /// the daemon's and so survives `kill_group_of(daemon_pid)` untouched.
    /// Call it as soon as the pid is known, by [`Self::adopt_home`]'s ordering.
    #[cfg(unix)]
    fn adopt_dog_pid(&mut self, pid: nix::unistd::Pid) {
        self.dog_pids.push(pid);
    }
}

impl Drop for DaemonGuard {
    /// On Windows the whole sweep collapses into `shep kill`: a job object
    /// takes the flock with the daemon. The unix arm exists because a sheep
    /// that outlives its daemon is an orphan only `kill(-pgid)` reaps.
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            for home in &self.homes {
                // Best-effort: a guard runs on a panic path where the daemon
                // may already be gone.
                let _ = std::process::Command::new(assert_cmd::cargo::cargo_bin("shep"))
                    .arg("--home")
                    .arg(home)
                    .arg("kill")
                    .output();
            }
        }

        #[cfg(unix)]
        {
            let panicking = std::thread::panicking();
            for home in &self.homes {
                match daemon_pid(home) {
                    Some(pid) => kill_group_of(pid),
                    // A success path unlinks the pidfile as its last act, so
                    // this is "already gone", not "never wrote one".
                    None if !panicking => {}
                    // On the panic path the case may have died inside the
                    // empty-pidfile window GUARD_PID_DEADLINE covers.
                    None => match wait_for_daemon_pid(home) {
                        Some(pid) => kill_group_of(pid),
                        None => eprintln!(
                            "DaemonGuard: no parseable daemon pid at {} after {GUARD_PID_DEADLINE:?}; \
                         if a daemon is still up it was NOT reaped",
                            home.display()
                        ),
                    },
                }

                if !panicking {
                    continue;
                }
                sweep_flock(home);
            }

            for pid in &self.dog_pids {
                kill_group_of(*pid);
            }
        }
    }
}

#[cfg(unix)]
/// SIGKILLs every process group named in `home`'s [`FIXTURE_PIDS`], resweeping
/// until [`GUARD_SWEEP_WINDOW`] expires.
///
/// A sheep records its pid as its script's first line, but `shep start`
/// reports `Online` off the spawn, so a case that panics straight after it
/// reaches here with the pid file still empty. Bounded rather than convergent:
/// no case tells this guard how many sheep to expect.
///
/// The daemon must already be dead: a sheep killed under a live supervisor is
/// one the restart brain brings straight back.
fn sweep_flock(home: &Path) {
    let start = Instant::now();
    loop {
        for pid in recorded_fixture_pids(home) {
            // `-pid`: every recorded pid leads its own group, so this
            // reaches its lambs. Re-signalling a dead one is an ESRCH no-op.
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(-pid.as_raw()),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        if start.elapsed() >= GUARD_SWEEP_WINDOW {
            return;
        }
        std::thread::sleep(GUARD_PID_POLL_INTERVAL);
    }
}

#[cfg(unix)]
/// One non-blocking attempt at the daemon pid recorded at `home`.
fn daemon_pid(home: &Path) -> Option<nix::unistd::Pid> {
    let text = std::fs::read_to_string(home.join("pids").join("shepd.pid")).ok()?;
    let raw: i32 = text.trim().parse().ok()?;
    Some(nix::unistd::Pid::from_raw(raw))
}

#[cfg(unix)]
/// [`daemon_pid`], retried until it answers or [`GUARD_PID_DEADLINE`] expires.
/// A live daemon fills the pidfile in `PidfileLock::record`; one that never
/// fills it has already exited.
fn wait_for_daemon_pid(home: &Path) -> Option<nix::unistd::Pid> {
    let start = Instant::now();
    loop {
        if let Some(pid) = daemon_pid(home) {
            return Some(pid);
        }
        if start.elapsed() >= GUARD_PID_DEADLINE {
            return None;
        }
        std::thread::sleep(GUARD_PID_POLL_INTERVAL);
    }
}

#[cfg(unix)]
/// SIGKILLs `pid`'s process group, or `pid` alone if it does not lead one.
///
/// Leadership is checked, not assumed: `-pid` against a non-leader reaches
/// somebody else's group, which in a test runner holds the harness. `ESRCH`
/// from `getpgid` means already reaped, and the leader-only fallback is then a
/// no-op.
fn kill_group_of(pid: nix::unistd::Pid) {
    let target = match nix::unistd::getpgid(Some(pid)) {
        Ok(pgid) if pgid == pid => nix::unistd::Pid::from_raw(-pid.as_raw()),
        _ => pid,
    };
    // ESRCH on an already-reaped daemon is the expected happy path.
    let _ = nix::sys::signal::kill(target, nix::sys::signal::Signal::SIGKILL);
}

#[cfg(unix)]
/// Every pid a fixture script recorded under `home`, in spawn order.
///
/// A missing file means the case started no sheep. An unparseable line is
/// skipped: this runs on a path that is already failing.
fn recorded_fixture_pids(home: &Path) -> Vec<nix::unistd::Pid> {
    let Ok(text) = std::fs::read_to_string(home.join(FIXTURE_PIDS)) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| line.trim().parse::<i32>().ok())
        .map(nix::unistd::Pid::from_raw)
        .collect()
}

#[cfg(unix)]
/// Reads the daemon pid recorded at `home`'s pidfile, the same path
/// `shep_daemon::boot::pidfile` builds.
fn read_daemon_pid(home: &Path) -> nix::unistd::Pid {
    let path = home.join("pids").join("shepd.pid");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("no pidfile at {}: {e}", path.display()));
    let raw: i32 = text
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("bad pidfile contents {text:?}: {e}"));
    nix::unistd::Pid::from_raw(raw)
}

#[cfg(unix)]
/// Asserts `pid` is the leader of its own process group, the
/// `Command::process_group(0)` contract `launch.rs` relies on to detach the
/// daemon from the parent's group and terminal. `std::process::Command`
/// exposes no getter for this, so a real spawn is the only check.
fn assert_group_leader(pid: nix::unistd::Pid) {
    assert_eq!(
        nix::unistd::getpgid(Some(pid)).unwrap(),
        pid,
        "the daemon must be its own process-group leader"
    );
}

// --- Dogs helpers -----------------------------------------------------

/// A port with nothing on it: bind `:0`, read what the OS chose, release it.
///
/// A stranger taking it before the dog binds is a loud loss: the dog refuses
/// to run and `shep dogs` reports it errored.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("the OS must have a free loopback port")
        .local_addr()
        .expect("a bound listener has an address")
        .port()
}

/// One attempt at a `GET /metrics` scrape against `addr`, over a plain
/// `std::net::TcpStream`: this workspace carries no HTTP crate.
///
/// Reads to EOF: the dog answers one request per connection and then drops the
/// stream, so the peer closing ends the response.
///
/// # Errors
/// Connection refused (nothing bound yet), or no full response within
/// [`METRICS_SCRAPE_READ_TIMEOUT`].
fn scrape_metrics(addr: std::net::SocketAddr) -> std::io::Result<String> {
    use std::io::{Read as _, Write as _};
    let mut stream = std::net::TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(METRICS_SCRAPE_READ_TIMEOUT))?;
    stream.write_all(b"GET /metrics HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")?;
    let mut body = String::new();
    stream.read_to_string(&mut body)?;
    Ok(body)
}

/// [`scrape_metrics`], retried until it answers or [`METRICS_SCRAPE_DEADLINE`]
/// expires, returning the last attempt's body (`""` if none connected). A
/// target that never comes up fails as an assertion on an empty string, never
/// a hang.
fn poll_metrics(addr: std::net::SocketAddr) -> String {
    let start = Instant::now();
    loop {
        if let Ok(body) = scrape_metrics(addr) {
            return body;
        }
        if start.elapsed() >= METRICS_SCRAPE_DEADLINE {
            return String::new();
        }
        std::thread::sleep(METRICS_SCRAPE_POLL_INTERVAL);
    }
}

/// One attempt at a request against a `shep serve` worker. `serve::worker`
/// answers `Connection: close`, so reading to EOF reads the whole response.
///
/// Returns the status code off the first line and everything after the blank
/// line as the body. Not a real HTTP parser: nothing this tier produces is
/// chunked.
///
/// # Errors
/// Connection refused (nothing bound yet), or no full response within
/// [`SERVE_HTTP_READ_TIMEOUT`].
fn http_get(
    addr: std::net::SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
) -> std::io::Result<(u16, String)> {
    use std::io::{Read as _, Write as _};
    let mut stream = std::net::TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(SERVE_HTTP_READ_TIMEOUT))?;
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes())?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw)?;
    let status = raw
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = raw
        .split_once("\r\n\r\n")
        .map_or("", |(_, body)| body)
        .to_string();
    Ok((status, body))
}

/// [`http_get`], retried until it answers or [`SERVE_HTTP_DEADLINE`]
/// expires, returning the last attempt's status and body either way
/// (`(0, "")` if every attempt failed to connect at all).
fn poll_http_get(
    addr: std::net::SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
) -> (u16, String) {
    let start = Instant::now();
    loop {
        if let Ok(answer) = http_get(addr, path, headers) {
            return answer;
        }
        if start.elapsed() >= SERVE_HTTP_DEADLINE {
            return (0, String::new());
        }
        std::thread::sleep(SERVE_HTTP_POLL_INTERVAL);
    }
}

#[cfg(unix)]
/// Runs `shep flock --format json` until it answers a `pid` for the dog named
/// `name`, or [`FLOCK_DEADLINE`] expires. `shep enable` returning success means
/// the `EnableDog` RPC landed, not that a pid is recorded.
///
/// `flock`, not `dogs`: `Response::Flock` carries both populations in one
/// array. Panics on expiry, since a `None` would leave a running dog
/// unregistered with the guard that exists to reap it.
fn wait_for_dog_pid(home: &Path, name: &str) -> nix::unistd::Pid {
    let flock = poll_flock_data(home, FLOCK_DEADLINE, |data| {
        data.as_array().is_some_and(|entries| {
            entries
                .iter()
                .any(|e| e["name"] == name && !e["pid"].is_null())
        })
    });
    let dog = flock
        .as_array()
        .and_then(|entries| entries.iter().find(|e| e["name"] == name))
        .unwrap_or_else(|| panic!("no entry named {name} in `shep flock`: {flock}"));
    let pid = dog["pid"]
        .as_i64()
        .unwrap_or_else(|| panic!("dog {name} has no pid after {FLOCK_DEADLINE:?}: {dog}"));
    nix::unistd::Pid::from_raw(i32::try_from(pid).expect("a real OS pid fits i32"))
}

/// Runs `shep bleats --no-follow` with `args` appended until its stdout is
/// non-empty or [`BLEATS_DEADLINE`] expires, returning the last `Output`.
///
/// The retry covers the gap between `shep start` returning and the daemon's
/// log pump writing the child's first line. Reading the log file directly
/// would tie this tier to a path rule an app's `out_file` overrides.
fn bleats_no_follow_until_written(home: &Path, args: &[&str]) -> Output {
    bleats_no_follow_until(home, args, |stdout| !stdout.is_empty())
}

/// [`bleats_no_follow_until_written`] for a caller that needs more than one
/// line: waits until every string in `needles` is in one reading.
///
/// A sheep's two streams reach two files the pump fills independently, so a
/// caller asserting on both would otherwise take the first reading with
/// either.
fn bleats_no_follow_until_contains(home: &Path, args: &[&str], needles: &[&str]) -> Output {
    bleats_no_follow_until(home, args, |stdout| {
        needles.iter().all(|needle| stdout.contains(needle))
    })
}

/// The shared retry loop: runs the command until `done` accepts its stdout or
/// [`BLEATS_DEADLINE`] expires.
fn bleats_no_follow_until(home: &Path, args: &[&str], done: impl Fn(&str) -> bool) -> Output {
    let start = Instant::now();
    loop {
        let output = shep(home)
            .arg("bleats")
            .arg("--no-follow")
            .args(args)
            .output()
            .unwrap();
        if done(&String::from_utf8_lossy(&output.stdout)) || start.elapsed() >= BLEATS_DEADLINE {
            return output;
        }
        std::thread::sleep(BLEATS_POLL_INTERVAL);
    }
}

/// Runs `shep flock --format json` until `done` accepts the whole `data`
/// array, or `deadline` expires, returning the last observation either way.
///
/// Returning rather than panicking on expiry keeps the failure the caller's
/// own assertion. The deadline is a parameter because the two real-clock cases
/// need one an order of magnitude past [`FLOCK_DEADLINE`].
/// Every attempt must succeed; the one case that signals a handover itself
/// polls through [`poll_flock_data_across_a_handover`].
fn poll_flock_data(
    home: &Path,
    deadline: Duration,
    done: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    poll_flock_until(home, deadline, false, done)
}

/// [`poll_flock_data`] for the one case that signals a handover itself and
/// then polls the shepherd being replaced.
///
/// The attempt whose reply is in flight at the `execve` fails with
/// [`DROPPED_REPLY`]; asserting on it turns a tolerated event into a panic.
/// One drop, and a second is still fatal: the poll is serial and the exec
/// happens once, so at most one accepted connection is open at the swap.
#[cfg(unix)]
fn poll_flock_data_across_a_handover(
    home: &Path,
    deadline: Duration,
    done: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    poll_flock_until(home, deadline, true, done)
}

/// The loop the two above share. `tolerate_one_drop` lets one attempt fail
/// with a connection the shepherd closed after accepting it.
///
/// A tolerated attempt costs a retry and nothing else: it consults neither
/// `done` nor `deadline`. The retry can land after `deadline` by one poll
/// interval and one command, and that is wanted: a drop at the edge that
/// ended the poll would be the flake this closes, one window narrower.
fn poll_flock_until(
    home: &Path,
    deadline: Duration,
    tolerate_one_drop: bool,
    done: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let start = Instant::now();
    let mut tolerance = tolerate_one_drop;
    loop {
        let output = shep(home)
            .arg("--format")
            .arg("json")
            .arg("flock")
            .output()
            .unwrap();
        if tolerance && !output.status.success() && closed_by_a_handover(&output) {
            tolerance = false;
            std::thread::sleep(FLOCK_POLL_INTERVAL);
            continue;
        }
        assert_success(&output);
        let envelope: serde_json::Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|e| panic!("flock stdout was not JSON: {e}"));
        let data = envelope["data"].clone();
        if done(&data) || start.elapsed() >= deadline {
            return data;
        }
        std::thread::sleep(FLOCK_POLL_INTERVAL);
    }
}

/// Whether `output` is a client whose connection the shepherd had accepted
/// when it replaced its own image. The two sentences and nothing else, so a
/// shepherd that is gone or refusing still fails the caller.
fn closed_by_a_handover(output: &Output) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr.contains(DROPPED_REPLY) || stderr.contains(DROPPED_HANDSHAKE)
}

/// [`poll_flock_data`] for the single-sheep cases: waits [`FLOCK_DEADLINE`]
/// and hands `done`, and the caller, that one sheep's `ProcessInfo` rather
/// than the array around it.
fn poll_flock(home: &Path, done: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
    poll_flock_data(home, FLOCK_DEADLINE, |data| done(&data[0]))[0].clone()
}

/// Runs `shep --format json describe <name>` until its lamb tree is non-empty
/// or `deadline` expires, returning the last `Output` either way.
///
/// `describe` walks the live process tree in its own handler, so the first
/// call after `start` races `/bin/sh` forking and exec'ing its trailing
/// `sleep`.
fn poll_describe_lambs(home: &Path, name: &str, deadline: Duration) -> Output {
    let start = Instant::now();
    loop {
        let output = shep(home)
            .arg("--format")
            .arg("json")
            .arg("describe")
            .arg(name)
            .output()
            .unwrap();
        assert_success(&output);
        let envelope: serde_json::Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|e| panic!("describe stdout was not JSON: {e}"));
        let has_lamb = envelope["data"][0]["lambs"]
            .as_array()
            .is_some_and(|lambs| !lambs.is_empty());
        if has_lamb || start.elapsed() >= deadline {
            return output;
        }
        std::thread::sleep(FLOCK_POLL_INTERVAL);
    }
}

/// The `data[]` element named `name`, for the cases that run a control sheep
/// beside the one under test. By name, since `data[0]`/`data[1]` would swap
/// meanings if id or app ordering moved.
fn sheep_named<'a>(data: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    data.as_array()
        .unwrap_or_else(|| panic!("flock data must be an array: {data}"))
        .iter()
        .find(|info| info["name"] == name)
        .unwrap_or_else(|| panic!("no sheep named {name} in the flock: {data}"))
}

// --- Dog index helpers -------------------------------------------------

/// Serves `response`, a complete raw HTTP response, once on an ephemeral
/// loopback port from a background thread, and returns its `http://` URL.
///
/// `SHEP_DOG_INDEX`'s loopback carve-out is what allows `http://` here: this
/// file drives the real binary, so there is no seam to skip the `https://`
/// check from.
fn serve_raw_response(response: String) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        if let Ok((mut stream, _peer)) = listener.accept() {
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    });
    format!("http://127.0.0.1:{}/dogs.json", addr.port())
}

/// [`serve_raw_response`] wrapping `body` as a well-formed 200.
fn serve_dog_index(body: &str) -> String {
    serve_raw_response(format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    ))
}

/// A two-entry community index shaped like the live `web/public/dogs.json`:
/// Spot, clean; and Rex, whose description carries a raw `\u{1b}[2J`
/// screen-clear for `dog_index::sanitise` to strip. Both are valid, so
/// `skipped` stays zero.
///
/// `parse_index` refuses a bare array, so the entries are wrapped in the
/// `$schema`/`version`/`dogs` object.
fn two_entry_index_json() -> String {
    serde_json::json!({
        "$schema": "https://shep-pm.com/dogs.schema.json",
        "version": 1,
        "dogs": [
            {
                "name": "Spot",
                "package": "shep-log-rotate",
                "adopt_as": "log-rotate",
                "description": "Rotates grown log files and asks the shepherd to reopen them.",
                "repo": "https://github.com/shep-pm/shep-log-rotate",
                "license": "MIT OR Apache-2.0",
                "category": "logs",
                "source": {
                    "kind": "cargo-git",
                    "url": "https://github.com/shep-pm/shep-log-rotate"
                }
            },
            {
                "name": "Rex",
                "package": "shep-watchdog",
                "adopt_as": "watchdog",
                "description": "Barks when a sheep stops answering.\u{1b}[2J",
                "repo": "https://github.com/example/shep-watchdog",
                "license": "Apache-2.0",
                "category": "health",
                "source": {
                    "kind": "go-install",
                    "module": "github.com/example/shep-watchdog"
                }
            }
        ]
    })
    .to_string()
}

// --- JSON fixture helpers -------------------------------------------------

/// Asserts `info`, one `data[]` element of an envelope, carries the dynamic
/// fields a real spawned sheep must have, then blanks them to `null` so the
/// rest can be compared against a committed fixture verbatim.
///
/// `pid`, `uptime_ms` and the tempdir-rooted `out_file`/`err_file` cannot be
/// pinned across runs, so each is asserted against its own shape first.
/// `samples` says whether this verb takes a live resource reading, which is
/// the assertion `memory_bytes` gets in place of a value.
fn normalize_process_info(info: &mut serde_json::Value, home: &Path, name: &str, samples: Samples) {
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
    match samples {
        Samples::Live => {
            let bytes = info["memory_bytes"].as_u64().unwrap_or_else(|| {
                panic!("memory_bytes must be a live reading off the host: {info}")
            });
            assert!(
                bytes > 0,
                "a running sheep's tree cannot be 0 bytes: {info}"
            );
        }
        Samples::None => assert!(
            info["memory_bytes"].is_null(),
            "a verb that takes no live sample must report no memory: {info}"
        ),
    }
    // `cpu_percent` needs a periodic baseline, so whether one exists depends on
    // the daemon living through a poll interval: a clock race either way.
    info["pid"] = serde_json::Value::Null;
    info["uptime_ms"] = serde_json::Value::Null;
    info["out_file"] = serde_json::Value::Null;
    info["err_file"] = serde_json::Value::Null;
    info["cpu_percent"] = serde_json::Value::Null;
    info["memory_bytes"] = serde_json::Value::Null;
    // `lambs[].pid` races the same way. `lambs[].name` stays: it is
    // deterministic once the walk has caught the lamb.
    if let Some(lambs) = info["lambs"].as_array_mut() {
        for lamb in lambs {
            lamb["pid"] = serde_json::Value::Null;
        }
    }
}

/// Whether the verb an envelope answers takes a live resource reading.
#[derive(Debug, Clone, Copy)]
enum Samples {
    /// `flock` and `describe`, which sample the host as they reply.
    Live,
    /// Every other verb answering with a `ProcessInfo`.
    None,
}

/// Parses `output.stdout` as a `flock`/`describe`/`start` envelope,
/// normalizes its one `data[]` element, and compares the result against the
/// committed fixture named `command`.
fn assert_envelope_matches_fixture(
    output: &Output,
    home: &Path,
    command: &str,
    sheep_name: &str,
    samples: Samples,
) {
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
    normalize_process_info(&mut envelope["data"][0], home, sheep_name, samples);
    assert_eq!(
        envelope,
        load_fixture(command),
        "{command} envelope drifted from its committed fixture"
    );
}

/// Asserts a failed `--format json` invocation kept `stdout` empty and put a
/// parseable `{"schema_version", "error": {"code", "message"}}` object on
/// `stderr`. Only this tier has two real streams.
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

#[cfg(unix)]
/// Also asserts the daemon is its own process-group leader, the
/// `Command::process_group(0)` contract `launch.rs` relies on and which
/// `std::process::Command` exposes no getter for.
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

#[cfg(unix)]
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

/// Three sheep and a stop of one, so the narrow answer and the full one differ
/// by row count as well as by content. The name assertion is on the exact set:
/// a `contains("alpha")` would pass on a one-row table too.
#[test]
fn a_lifecycle_verb_prints_the_whole_flock_and_json_still_prints_what_it_touched() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();

    for name in ["alpha", "gamma", "beta"] {
        let started = shep(home)
            .arg("start")
            .arg(&script)
            .arg("--name")
            .arg(name)
            .output()
            .unwrap();
        guard.adopt_home(home);
        assert_success(&started);
    }

    let stopped = shep(home).arg("stop").arg("alpha").output().unwrap();
    assert_success(&stopped);
    let printed = String::from_utf8(stopped.stdout).unwrap();
    let named: Vec<&str> = printed
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .filter(|word| *word != "NAME")
        .collect();
    assert_eq!(
        named,
        ["alpha", "beta", "gamma"],
        "stopping one sheep prints the whole flock, in name order: {printed}"
    );

    let json = shep(home)
        .arg("--format")
        .arg("json")
        .arg("stop")
        .arg("beta")
        .output()
        .unwrap();
    assert_success(&json);
    let envelope: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    let rows = envelope["data"].as_array().unwrap();
    let names: Vec<&str> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        ["beta"],
        "the machine surface still answers what it touched: {envelope}"
    );

    graceful_kill(home);
}

// --- Case 3 --------------------------------------------------------------

#[cfg(unix)]
/// `flock(2)` makes the race safe daemon-side and `connect_or_spawn`
/// client-side: the loser's child exits carrying `DAEMON_ALREADY_RUNNING` and
/// the client keeps probing rather than surfacing that as an error.
///
/// A `std::sync::Barrier` holds the two racers until both are ready, or
/// scheduling could let one finish before the other starts. Each is collected
/// over a channel, since `JoinHandle::join` has no bounded form and a stuck
/// racer joined directly would stop the suite.
#[test]
fn concurrent_cold_starts_produce_exactly_one_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();
    // Two racers means two Outputs and no single point that precedes every
    // panic path, so the earliest safe point is before either thread starts.
    guard.adopt_home(&home);

    let names = ["racer-a", "racer-b"];
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(names.len()));
    let (finished, racers) = std::sync::mpsc::channel();
    for name in names {
        let home = home.clone();
        let script = script.clone();
        let barrier = std::sync::Arc::clone(&barrier);
        let finished = finished.clone();
        std::thread::spawn(move || {
            barrier.wait(); // both racers launch together
            let output = shep(&home)
                .arg("start")
                .arg(&script)
                .arg("--name")
                .arg(name)
                .output()
                .unwrap();
            // A closed receiver means the case already gave up on this racer.
            let _ = finished.send((name, output));
        });
    }
    drop(finished); // the racers hold the only senders that matter

    let outputs: Vec<(&str, Output)> = (0..names.len())
        .map(|_| {
            racers
                .recv_timeout(RACER_DEADLINE)
                .expect("a racer never came back; see RACER_DEADLINE")
        })
        .collect();
    for (name, output) in &outputs {
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

#[cfg(unix)]
/// Envelopes for `flock`, `describe`, `start` and `ping` are compared
/// structurally after normalizing what a real spawn cannot pin;
/// `bleats --no-follow` is one object with no envelope, compared byte for
/// byte.
///
/// One sheep named "fixture" at id 0, since a fresh home allocates ids from
/// zero. Its single stdout marker is what keeps `bleats` to the one line the
/// fixture pins.
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
    assert_envelope_matches_fixture(&start_out, home, "start", "fixture", Samples::None);

    let flock_out = shep(home)
        .arg("--format")
        .arg("json")
        .arg("flock")
        .output()
        .unwrap();
    assert_success(&flock_out);
    assert_envelope_matches_fixture(&flock_out, home, "flock", "fixture", Samples::Live);

    let describe_out = poll_describe_lambs(home, "fixture", FLOCK_DEADLINE);
    assert_envelope_matches_fixture(&describe_out, home, "describe", "fixture", Samples::Live);

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
    // `home` and `socket` are a tempdir here, so assert they are right and
    // then null them: a fixture cannot hold a path that changes every run.
    assert_eq!(
        ping_envelope["data"]["home"].as_str().unwrap(),
        home.to_str().unwrap(),
        "ping must name the home it probed"
    );
    assert!(
        ping_envelope["data"]["socket"]
            .as_str()
            .unwrap()
            .starts_with(home.to_str().unwrap()),
        "ping's socket must sit under that home"
    );
    ping_envelope["data"]["home"] = serde_json::Value::Null;
    ping_envelope["data"]["socket"] = serde_json::Value::Null;
    // Asserted and then nulled: a frozen version would turn every release bump
    // into a red test.
    assert_eq!(
        ping_envelope["data"]["daemon_version"].as_str().unwrap(),
        env!("CARGO_PKG_VERSION"),
        "ping must report this build's own version"
    );
    ping_envelope["data"]["daemon_version"] = serde_json::Value::Null;
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

/// A selector matching nothing exits `NotFound`; the malformed `/[/` exits
/// `Usage` (`/unclosed` would parse as a literal name and exit `NotFound`); a
/// daemonless `--home` exits `DaemonUnreachable` and an absent one exits
/// `Usage`. For each, stdout stays empty and stderr parses as JSON.
///
/// The first two need a live daemon: every non-`Start` verb connects before
/// parsing its selector, so a cold `$SHEP_HOME` would exit
/// `DaemonUnreachable` first and hide both.
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

    // A home that exists but has never had a daemon: `flock` never autostarts,
    // so nothing is listening for the whole invocation. Created, because an
    // absent `--home` is its own refusal and never reaches the connect.
    let cold = tempfile::tempdir().unwrap();
    let quiet_home = cold.path().join("no-daemon-here");
    std::fs::create_dir_all(&quiet_home).unwrap();
    let unreachable = shep(&quiet_home)
        .arg("--format")
        .arg("json")
        .arg("flock")
        .output()
        .unwrap();
    assert_json_error(&unreachable, 5, "daemon_unreachable");

    // An absent `--home` is a usage error, not an unreachable daemon: there is
    // no flock at that path, and creating one would leave a second empty home.
    let missing_home = cold.path().join("gone");
    let absent = shep(&missing_home)
        .arg("--format")
        .arg("json")
        .arg("flock")
        .output()
        .unwrap();
    assert_json_error(&absent, 2, "usage");
    assert!(
        !missing_home.exists(),
        "a refused --home must be left on disk exactly as it was found"
    );

    // Neither of those homes ever had a daemon, so there is nothing to kill.

    graceful_kill(home);
}

// --- Case 6 --------------------------------------------------------------

#[test]
fn kill_stops_the_daemon_and_removes_the_socket() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();

    let boot = shep(home).arg("start").arg(&script).output().unwrap();
    guard.adopt_home(home);
    assert_success(&boot);

    // On unix the control address is a socket file the daemon unlinks. On
    // Windows it is a named pipe with no directory entry, so `Path::exists`
    // would read `false` before the kill and pass vacuously.
    #[cfg(unix)]
    let socket = home.join("run").join("shep.sock");
    #[cfg(unix)]
    assert!(socket.exists(), "precondition: the daemon is up");
    #[cfg(windows)]
    assert_success(&shep(home).arg("flock").output().unwrap());

    let kill = shep(home).arg("kill").output().unwrap();
    assert_success(&kill);

    #[cfg(unix)]
    assert!(!socket.exists(), "kill must remove the socket file");
    #[cfg(windows)]
    {
        // `shep flock` against a departed shepherd exits `DaemonUnreachable`,
        // the same fact the missing socket file states on unix.
        let after = shep(home).arg("flock").output().unwrap();
        assert!(
            !after.status.success(),
            "kill must leave nothing answering on the control pipe; stderr={}",
            String::from_utf8_lossy(&after.stderr)
        );
    }
}

// --- Case 7 --------------------------------------------------------------

/// Both of a sheep's streams by default, only the requested one under `--out`.
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

    let both = bleats_no_follow_until_contains(
        home,
        &["all"],
        &["bleater-out-marker", "bleater-err-marker"],
    );
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

/// `create`-mode rotation: rename the live log, run `shep reopen`, and the
/// sheep's next line reaches the recreated path.
///
/// Both directions. The second line appearing rules out a reopen that did
/// nothing; the first one being absent rules out a `bleats` that found the
/// archive, or a pump still holding the old inode.
#[test]
fn reopen_puts_a_rotated_log_back_where_bleats_can_read_it() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let gate = home.join("rotated");
    let script = write_rotating_script(&dir, &gate);
    let mut guard = DaemonGuard::default();

    let boot = shep(home)
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("rotator")
        .output()
        .unwrap();
    guard.adopt_home(home);
    assert_success(&boot);

    // Through the reading verb, so the precondition is the same observation the
    // assertion at the bottom makes.
    let before = bleats_no_follow_until_written(home, &["all"]);
    let printed = String::from_utf8_lossy(&before.stdout);
    assert!(
        printed.contains(ROTATE_BEFORE),
        "precondition: the sheep's first line must be readable before the \
         rotation: stdout={printed}"
    );

    let online = poll_flock(home, |info| info["status"] == "online");
    let out_file = PathBuf::from(
        online["out_file"]
            .as_str()
            .unwrap_or_else(|| panic!("the daemon reports its own log paths: {online}")),
    );
    let archive = out_file.with_extension("log.1");
    std::fs::rename(&out_file, &archive).unwrap();
    assert!(!out_file.exists(), "sanity: the rename really moved it");

    // No selector, the verb's default, as a `postrotate` stanza calls it. The
    // `command` label is asserted because `reopen` and `flush` render an
    // identical table, so the labels are swappable with nothing else moving.
    let reopened = shep(home)
        .arg("reopen")
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();
    assert_success(&reopened);
    let envelope: serde_json::Value = serde_json::from_slice(&reopened.stdout).unwrap();
    assert_eq!(
        envelope["command"], "reopen",
        "a reopen's envelope must say so: {envelope}"
    );

    // Opened only now, so the line below cannot predate the reopen.
    std::fs::write(&gate, "").unwrap();

    let after = bleats_no_follow_until_written(home, &["all"]);
    assert_eq!(
        after.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&after.stderr)
    );
    let stdout = String::from_utf8_lossy(&after.stdout);
    assert!(
        stdout.contains(ROTATE_AFTER),
        "a rotated sheep's next line must reach the recreated path: stdout={stdout}"
    );
    assert!(
        !stdout.contains(ROTATE_BEFORE),
        "the recreated log starts empty — the first line belongs to the \
         archive now: stdout={stdout}"
    );
    assert_eq!(
        unstamped_file(&archive),
        format!("{ROTATE_BEFORE}\n"),
        "the renamed file must stop growing the moment the handle is swapped"
    );

    graceful_kill(home);
}

/// The chain from a pump that cannot open a path again, through
/// `SupervisorError::ReopenFailed` and `rpc_error`'s `Internal`, to exit 9.
///
/// A directory in stdout's place is the failure with no permission games in
/// it: `open(2)` on a directory fails for every uid. stderr's path is left
/// alone, so the message must name stdout's and only stdout's.
#[test]
fn a_reopen_that_cannot_open_a_path_again_exits_internal() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    const MARKER: &str = "blocked-out-marker";
    let script = write_logging_script(&dir, MARKER, None);
    let mut guard = DaemonGuard::default();

    let boot = shep(home)
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("blocked")
        .output()
        .unwrap();
    guard.adopt_home(home);
    assert_success(&boot);

    // `online` says the daemon spawned the child, not that the pump opened the
    // file. Without this wait the rename fails ENOENT.
    let written = bleats_no_follow_until_written(home, &["all"]);
    let printed = String::from_utf8_lossy(&written.stdout);
    assert!(
        printed.contains(MARKER),
        "precondition: the pump must have opened the log before the rotation \
         renames it: stdout={printed}"
    );

    // Off the daemon's own snapshot, so the test cannot disagree about which
    // file it is blocking.
    let online = poll_flock(home, |info| info["status"] == "online");
    let out_file = PathBuf::from(
        online["out_file"]
            .as_str()
            .unwrap_or_else(|| panic!("the daemon reports its own log paths: {online}")),
    );

    // Renamed, not deleted: a real rotation leaves the pump holding an inode
    // under a different name, with the live path unopenable.
    std::fs::rename(&out_file, out_file.with_extension("log.1")).unwrap();
    std::fs::create_dir(&out_file).unwrap();

    let refused = shep(home)
        .arg("reopen")
        .arg("blocked")
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();
    assert_json_error(&refused, 9, "internal");
    let err: serde_json::Value = serde_json::from_slice(&refused.stderr).unwrap();
    let message = err["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains(out_file.to_str().unwrap()),
        "the operator's one message must name the path that failed: {err}"
    );
    // The whole `<name> (id <id>)` prefix: the log path already contains the
    // name, so a bare name check would hold against a message naming no sheep.
    assert!(
        message.contains(&format!("blocked (id {})", online["id"])),
        "and the sheep it belongs to: {err}"
    );

    // Out of the daemon's way before the shutdown that follows, so nothing
    // downstream trips over a directory where a log file belongs.
    std::fs::remove_dir(&out_file).unwrap();
    graceful_kill(home);
}

/// `copytruncate`-mode rotation: an external rotator copies the live log aside
/// and empties it in place, telling the daemon nothing.
///
/// It works because a log file is opened `O_APPEND`, so every write seeks to
/// end of file and the next one lands at offset 0. The file's length is the
/// whole assertion: `bleats` prints the line either way, since a sparse hole
/// reads back as NUL bytes in front of it.
#[test]
fn an_external_copytruncate_leaves_the_next_line_at_offset_zero() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let gate = home.join("copied");
    let script = write_rotating_script(&dir, &gate);
    let mut guard = DaemonGuard::default();

    let boot = shep(home)
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("truncated")
        .output()
        .unwrap();
    guard.adopt_home(home);
    assert_success(&boot);

    // Through the reading verb, so the first line is known to be on disk before
    // the rotator below copies it.
    let before = bleats_no_follow_until_written(home, &["all"]);
    let printed = String::from_utf8_lossy(&before.stdout);
    assert!(
        printed.contains(ROTATE_BEFORE),
        "precondition: the sheep's first line must be readable before the \
         rotation: stdout={printed}"
    );

    // Off the daemon's own snapshot, so the test cannot disagree about which
    // file this is.
    let online = poll_flock(home, |info| info["status"] == "online");
    let out_file = PathBuf::from(
        online["out_file"]
            .as_str()
            .unwrap_or_else(|| panic!("the daemon reports its own log paths: {online}")),
    );

    // `logrotate copytruncate` spelled out. Nothing here is a shep verb, so
    // the daemon is never told and the pump holds the same inode at size zero.
    let archive = out_file.with_extension("log.1");
    std::fs::copy(&out_file, &archive).unwrap();
    std::fs::File::create(&out_file).unwrap();
    assert_eq!(
        unstamped_file(&archive),
        format!("{ROTATE_BEFORE}\n"),
        "sanity: the copy really took the line the truncate is about to drop"
    );
    assert_eq!(
        std::fs::metadata(&out_file).unwrap().len(),
        0,
        "sanity: the truncate really emptied it"
    );

    // Opened only now, so the line below cannot predate the truncation.
    std::fs::write(&gate, "").unwrap();

    let after = bleats_no_follow_until_written(home, &["all"]);
    assert_eq!(
        after.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&after.stderr)
    );
    let stdout = String::from_utf8_lossy(&after.stdout);
    assert!(
        stdout.contains(ROTATE_AFTER),
        "a truncated sheep must go on logging into the same file: stdout={stdout}"
    );
    // The line above is all the sheep wrote after the truncation, and the loop
    // that read it back already waited for it to reach disk.
    assert_eq!(
        std::fs::metadata(&out_file).unwrap().len(),
        // Stamp, line, newline: the claim is that the file holds one line's
        // worth of bytes with no hole in front of it.
        (shep_core::logstamp::LOG_STAMP_BYTES + ROTATE_AFTER.len() + 1) as u64,
        "the sheep's next line must land at offset 0 of the emptied file: a \
         handle that kept its offset across an external truncation would \
         leave a hole the size of what was emptied in front of it, and \
         `bleats` would print the line just the same"
    );

    graceful_kill(home);
}

/// [`ROTATE_BEFORE`] being gone proves the truncate happened; [`ROTATE_AFTER`]
/// arriving through the same untouched handle proves it survived.
///
/// The file's length proves where that line landed: `O_APPEND` puts it at
/// offset 0, while a preserved offset would put it past a sparse hole the
/// reading verb prints the same either way.
#[test]
fn flush_empties_a_log_the_sheep_goes_on_appending_to() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let gate = home.join("flushed");
    let script = write_rotating_script(&dir, &gate);
    let mut guard = DaemonGuard::default();

    let boot = shep(home)
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("flusher")
        .output()
        .unwrap();
    guard.adopt_home(home);
    assert_success(&boot);

    // Through the reading verb, so the precondition is the same observation the
    // assertions below make.
    let before = bleats_no_follow_until_written(home, &["all"]);
    let printed = String::from_utf8_lossy(&before.stdout);
    assert!(
        printed.contains(ROTATE_BEFORE),
        "precondition: the sheep's first line must be readable before the \
         flush: stdout={printed}"
    );

    // Off the daemon's own snapshot, so the test cannot disagree about which
    // file this is.
    let online = poll_flock(home, |info| info["status"] == "online");
    let out_file = PathBuf::from(
        online["out_file"]
            .as_str()
            .unwrap_or_else(|| panic!("the daemon reports its own log paths: {online}")),
    );

    // The selector is explicit because the verb requires one. `--format json`
    // for the `command` label, since `flush` and `reopen` render one table.
    let flushed = shep(home)
        .arg("flush")
        .arg("all")
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();
    assert_success(&flushed);
    let envelope: serde_json::Value = serde_json::from_slice(&flushed.stdout).unwrap();
    assert_eq!(
        envelope["command"], "flush",
        "a flush's envelope must say so: {envelope}"
    );

    // Opened only now, so the line below cannot predate the flush.
    std::fs::write(&gate, "").unwrap();

    let after = bleats_no_follow_until_written(home, &["all"]);
    assert_eq!(
        after.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&after.stderr)
    );
    let stdout = String::from_utf8_lossy(&after.stdout);
    assert!(
        stdout.contains(ROTATE_AFTER),
        "a flushed sheep must go on logging into the same file: stdout={stdout}"
    );
    assert!(
        !stdout.contains(ROTATE_BEFORE),
        "everything written before the flush is gone: stdout={stdout}"
    );
    // The line above is all the sheep wrote after the flush, and the loop that
    // read it back already waited for it to reach disk.
    assert_eq!(
        std::fs::metadata(&out_file).unwrap().len(),
        // Stamp, line, newline: the claim is that the file holds one line's
        // worth of bytes with no hole in front of it.
        (shep_core::logstamp::LOG_STAMP_BYTES + ROTATE_AFTER.len() + 1) as u64,
        "the sheep's next line must land at offset 0 of the emptied file: a \
         handle that kept its offset across the truncate would leave a hole \
         the size of what was emptied in front of it, and `bleats` would \
         print the line just the same"
    );

    graceful_kill(home);
}

/// The flock half runs first, while the shepherd's own logs still hold a
/// marker only this test wrote: `flush all` must leave it byte for byte, and
/// `flush --daemon` must leave the sheep's log untouched.
///
/// The daemon holds fd 1 on the same inode and writes nothing to stdout, so
/// nothing races the marker.
#[test]
fn a_daemon_flush_and_a_flock_flush_never_reach_each_others_files() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let gate = home.join("flushed");
    let script = write_rotating_script(&dir, &gate);
    let mut guard = DaemonGuard::default();

    let boot = shep(home)
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("flusher")
        .output()
        .unwrap();
    guard.adopt_home(home);
    assert_success(&boot);

    let online = poll_flock(home, |info| info["status"] == "online");
    let out_file = PathBuf::from(
        online["out_file"]
            .as_str()
            .unwrap_or_else(|| panic!("the daemon reports its own log paths: {online}")),
    );
    // Read back rather than assumed: this is the precondition both halves of
    // the case are checked against.
    let before = bleats_no_follow_until_written(home, &["all"]);
    assert!(
        String::from_utf8_lossy(&before.stdout).contains(ROTATE_BEFORE),
        "precondition: the sheep must have logged something to lose"
    );

    const MARKER: &[u8] = b"a line only the shepherd's own log holds\n";
    let shepd_out = home.join("logs").join("shepd.out.log");
    let shepd_err = home.join("logs").join("shepd.err.log");
    std::fs::write(&shepd_out, MARKER).unwrap();
    std::fs::write(&shepd_err, MARKER).unwrap();

    let flock_half = shep(home).arg("flush").arg("all").output().unwrap();
    assert_success(&flock_half);
    assert_eq!(
        std::fs::metadata(&out_file).unwrap().len(),
        0,
        "the flock half must still empty the sheep it named"
    );
    // Table mode: the paths ride the JSON whatever the table does, so only the
    // default rendering can show an operator losing them.
    let printed = String::from_utf8_lossy(&flock_half.stdout);
    assert!(
        printed.contains(&out_file.display().to_string()),
        "a flush table must name the files it emptied: {printed}"
    );
    assert_eq!(
        std::fs::read(&shepd_out).unwrap(),
        MARKER,
        "a flock flush must not reach the shepherd's own stdout log"
    );
    assert_eq!(
        std::fs::read(&shepd_err).unwrap(),
        MARKER,
        "a flock flush must not reach the shepherd's own stderr log"
    );

    // The sheep has written nothing since the truncate above, so its log is
    // refilled first and "untouched" is a fact with bytes behind it.
    std::fs::write(&gate, "").unwrap();
    let after = bleats_no_follow_until_written(home, &["all"]);
    assert!(
        String::from_utf8_lossy(&after.stdout).contains(ROTATE_AFTER),
        "the sheep must have written again before the --daemon flush"
    );
    let sheep_len = std::fs::metadata(&out_file).unwrap().len();
    assert!(sheep_len > 0, "precondition: the sheep's log is not empty");

    let daemon_half = shep(home)
        .arg("flush")
        .arg("--daemon")
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();
    assert_success(&daemon_half);
    let envelope: serde_json::Value = serde_json::from_slice(&daemon_half.stdout).unwrap();
    assert_eq!(envelope["command"], "flush", "{envelope}");
    let files: Vec<&str> = envelope["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["file"].as_str().unwrap())
        .collect();
    assert!(
        files.contains(&shepd_out.display().to_string().as_str())
            && files.contains(&shepd_err.display().to_string().as_str()),
        "the answer must name both files it emptied: {envelope}"
    );

    assert_eq!(std::fs::metadata(&shepd_out).unwrap().len(), 0);
    assert_eq!(std::fs::metadata(&shepd_err).unwrap().len(), 0);
    assert_eq!(
        std::fs::metadata(&out_file).unwrap().len(),
        sheep_len,
        "a --daemon flush must not reach any sheep's log"
    );

    graceful_kill(home);
}

/// The files belong to the CLI, since `launch::launch_command` creates them,
/// so there is nothing to ask. The socket is asserted too: a `connect_or_spawn`
/// here would autostart a daemon in order to be told to do nothing.
#[test]
fn a_daemon_flush_needs_no_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let logs = home.join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(
        logs.join("shepd.out.log"),
        b"left behind by a dead shepherd",
    )
    .unwrap();

    let flushed = shep(home).arg("flush").arg("--daemon").output().unwrap();

    assert_success(&flushed);
    assert_eq!(
        std::fs::metadata(logs.join("shepd.out.log")).unwrap().len(),
        0
    );
    assert!(
        !home.join("run").join("shep.sock").exists(),
        "this verb must not autostart a daemon to empty files the CLI owns"
    );
}

/// A `default_value` on the selector would make a bare `shep flush` empty
/// every log in the flock and exit 0. Clap must refuse before anything
/// connects, which the socket assertion is for.
#[test]
fn flush_without_a_selector_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let bare = shep(dir.path()).arg("flush").output().unwrap();

    assert_eq!(
        bare.status.code(),
        Some(2),
        "clap's usage exit code; stdout={}",
        String::from_utf8_lossy(&bare.stdout)
    );
    assert!(
        !dir.path().join("run").join("shep.sock").exists(),
        "a usage error must not have autostarted a daemon"
    );
}

// --- Case 8 --------------------------------------------------------------

#[cfg(unix)]
/// Asserted on the socket file's location, not on exit 0, so a child that
/// re-resolved `$SHEP_HOME` from the ambient environment and bound elsewhere
/// still fails.
///
/// Needs `env_remove` and a hand-built argv, so it cannot use the [`shep`]
/// helper but borrows [`CMD_TIMEOUT`]. That timeout reaps no daemon: the
/// launched child has its own process group, so a kill reaches the CLI only.
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
        .timeout(CMD_TIMEOUT)
        .output()
        .unwrap();

    // Registered before anything that can panic: a failed autostart is when a
    // daemon is most likely to be left behind.
    guard.adopt_home(dir.path());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let socket = dir.path().join("run").join("shep.sock");
    assert!(
        socket.exists(),
        "the daemon bound somewhere other than --home"
    );

    graceful_kill(dir.path());
}

// --- Case 9 --------------------------------------------------------------

#[cfg(unix)]
/// The watched tree is its own [`TempDir`], never this case's `$SHEP_HOME`: a
/// watch rooted there would see [`FIXTURE_PIDS`] grow on each spawn and
/// restart on its own sheep.
#[test]
fn a_write_under_a_watched_tree_restarts_the_sheep() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let watched = tempfile::tempdir().unwrap();
    let script = write_test_script(&dir);
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"watcher\"\nscript = '{}'\ncwd = '{}'\nwatch = true\n",
            script.display(),
            watched.path().display(),
        ),
    );
    let mut guard = DaemonGuard::default();

    let boot = shep(home).arg("start").arg(&flockfile).output().unwrap();
    guard.adopt_home(home);
    assert_success(&boot);

    let before = poll_flock(home, |info| info["status"] == "online");
    assert_eq!(before["restarts"], 0, "precondition: {before}");

    std::fs::write(watched.path().join("app.txt"), "changed").unwrap();

    let after = poll_flock(home, |info| info["restarts"] == 1);
    assert_eq!(
        after["restarts"], 1,
        "a write under the watched tree must restart the sheep exactly once: {after}"
    );

    graceful_kill(home);
}

// --- Case 10 -------------------------------------------------------------

#[cfg(unix)]
/// A dot-file followed by a full [`FLOCK_DEADLINE`] of quiet is also what a
/// watcher that was never armed produces, so a plain file is written
/// afterwards and its restart must land. Two writes, exactly one restart.
#[test]
fn a_write_to_a_dot_file_under_a_watched_tree_restarts_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let watched = tempfile::tempdir().unwrap();
    let script = write_test_script(&dir);
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"watcher\"\nscript = '{}'\ncwd = '{}'\nwatch = true\n",
            script.display(),
            watched.path().display(),
        ),
    );
    let mut guard = DaemonGuard::default();

    let boot = shep(home).arg("start").arg(&flockfile).output().unwrap();
    guard.adopt_home(home);
    assert_success(&boot);

    let before = poll_flock(home, |info| info["status"] == "online");
    assert_eq!(before["restarts"], 0, "precondition: {before}");

    std::fs::write(watched.path().join(".hidden.swp"), "editor churn").unwrap();
    // Polls for the restart that must not come, for the same deadline the
    // positive case gives the one that must: `done` never accepts.
    let quiet = poll_flock(home, |_| false);
    assert_eq!(
        quiet["restarts"], 0,
        "a dot-file is ignored by default and must not restart anything: {quiet}"
    );

    std::fs::write(watched.path().join("app.txt"), "changed").unwrap();
    let after = poll_flock(home, |info| info["restarts"] == 1);
    assert_eq!(
        after["restarts"], 1,
        "the watcher must have been armed and delivering all along: {after}"
    );

    graceful_kill(home);
}

// --- Case 11 -------------------------------------------------------------

#[cfg(unix)]
/// The only tier that exercises the real fd-3 channel end to end; every other
/// test of this gate hands the supervisor a `ChildMessage` directly.
///
/// `listen_timeout` is raised far above its 3000ms default because the daemon
/// takes a `wait_ready` sheep `Online` on elapse anyway, which would make the
/// observation window and the timeout window the same window.
#[test]
fn a_wait_ready_sheep_goes_online_only_once_it_signals_ready() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let sentinel = dir.path().join("go");
    let script = write_ready_script(&dir, &sentinel);
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"gated\"\nscript = '{}'\nwait_ready = true\nlisten_timeout = \"120s\"\n",
            script.display(),
        ),
    );
    let mut guard = DaemonGuard::default();

    let boot = shep(home)
        .arg("--format")
        .arg("json")
        .arg("start")
        .arg(&flockfile)
        .output()
        .unwrap();
    guard.adopt_home(home);
    assert_success(&boot);

    let envelope: serde_json::Value = serde_json::from_slice(&boot.stdout).unwrap();
    assert_eq!(
        envelope["data"][0]["status"], "starting",
        "a wait_ready sheep must not be online before it signals: {envelope}"
    );

    std::fs::write(&sentinel, "").unwrap();

    let ready = poll_flock(home, |info| info["status"] == "online");
    assert_eq!(
        ready["status"], "online",
        "the sheep must reach online once it writes ready to fd 3: {ready}"
    );

    graceful_kill(home);
}

// --- Case 12 -------------------------------------------------------------

#[cfg(unix)]
/// Exit `4`, JSON on stderr, and the offending pattern in the message. Spans
/// `normalize`'s rejection, the daemon's `InvalidConfig` over RPC, and the
/// CLI's exit code. Asserted on the pattern, not the wording, which is
/// croner's.
#[test]
fn a_bad_cron_pattern_is_a_config_error() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script = write_test_script(&dir);
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"crony\"\nscript = '{}'\ncron_restart = \"not a cron\"\n",
            script.display(),
        ),
    );
    let mut guard = DaemonGuard::default();

    let output = shep(home)
        .arg("--format")
        .arg("json")
        .arg("start")
        .arg(&flockfile)
        .output()
        .unwrap();
    guard.adopt_home(home);

    assert_json_error(&output, 4, "invalid_config");
    let err: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    let message = err["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("not a cron"),
        "the rejection must name the offending pattern: {err}"
    );

    graceful_kill(home);
}

// --- Case 13 -------------------------------------------------------------

#[cfg(unix)]
/// Exit `4`, JSON on stderr, and the offending target in the message. The
/// daemon's prober carries no TLS stack, and a probe failing every poll would
/// look like a down app, so the target is refused at config time. This case
/// configures the readiness probe; `normalize`'s unit tier covers liveness.
#[test]
fn an_https_probe_target_is_a_config_error() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script = write_test_script(&dir);
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"probed\"\nscript = '{}'\n\
             readiness_probe = {{ kind = \"http\", target = \"https://localhost:8443/health\" }}\n",
            script.display(),
        ),
    );
    let mut guard = DaemonGuard::default();

    let output = shep(home)
        .arg("--format")
        .arg("json")
        .arg("start")
        .arg(&flockfile)
        .output()
        .unwrap();
    guard.adopt_home(home);

    assert_json_error(&output, 4, "invalid_config");
    let err: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    let message = err["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("https://localhost:8443/health"),
        "the rejection must name the offending target: {err}"
    );

    graceful_kill(home);
}

// --- Case 14 -------------------------------------------------------------

#[cfg(unix)]
/// The only place the cron subsystem runs on `SystemClock`; every other cron
/// test drives `TestClock` over a paused runtime.
///
/// `unscheduled` is the control: same script, same daemon, no `cron_restart`,
/// so a restart from the script exiting would move both counters. Its
/// [`SLOW_SCRIPT_SLEEP_SECS`] sleep outlasts [`CRON_DEADLINE`] twice over.
/// Costs 26s to 61s, a uniform draw on the minute the arming lands in.
#[test]
fn a_cron_occurrence_restarts_a_sheep_on_the_real_clock() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script = write_slow_script(&dir);
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"minutely\"\nscript = '{script}'\ncron_restart = \"* * * * *\"\n\n\
             [[app]]\nname = \"unscheduled\"\nscript = '{script}'\n",
            script = script.display(),
        ),
    );
    let mut guard = DaemonGuard::default();

    let boot = shep(home).arg("start").arg(&flockfile).output().unwrap();
    guard.adopt_home(home);
    assert_success(&boot);

    let before = poll_flock_data(home, FLOCK_DEADLINE, |data| {
        sheep_named(data, "minutely")["status"] == "online"
            && sheep_named(data, "unscheduled")["status"] == "online"
    });
    assert_eq!(
        sheep_named(&before, "minutely")["restarts"],
        0,
        "precondition: {before}"
    );
    assert_eq!(
        sheep_named(&before, "unscheduled")["restarts"],
        0,
        "precondition: {before}"
    );

    let after = poll_flock_data(home, CRON_DEADLINE, |data| {
        sheep_named(data, "minutely")["restarts"] == 1
    });
    assert_eq!(
        sheep_named(&after, "minutely")["restarts"],
        1,
        "a `* * * * *` occurrence must restart the sheep within one real minute: {after}"
    );
    assert_eq!(
        sheep_named(&after, "unscheduled")["restarts"],
        0,
        "the same script with no cron_restart must not have moved: a restart both sheep \
         share is the script exiting, not an occurrence firing: {after}"
    );

    graceful_kill(home);
}

// --- Case 15 -------------------------------------------------------------

#[cfg(unix)]
/// The only place `PollingEnforcer` and `SysinfoSampler` run together on real
/// time against a real spawned process.
///
/// `unlimited` is the control: same ballooning script, same daemon, no
/// `max_memory`, so a restart caused by the shell dying under its own
/// allocation would move both counters. Its [`SLOW_SCRIPT_SLEEP_SECS`] sleep
/// outlasts [`BREACH_DEADLINE`] five times over. Costs about 16s, one
/// `MEMORY_POLL_INTERVAL` plus a restart.
#[test]
fn a_real_memory_breach_restarts_a_sheep() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script = write_ballooning_script(&dir);
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"greedy\"\nscript = '{script}'\nmax_memory = \"{BREACH_LIMIT}\"\n\n\
             [[app]]\nname = \"unlimited\"\nscript = '{script}'\n",
            script = script.display(),
        ),
    );
    let mut guard = DaemonGuard::default();

    let boot = shep(home).arg("start").arg(&flockfile).output().unwrap();
    guard.adopt_home(home);
    assert_success(&boot);

    let before = poll_flock_data(home, FLOCK_DEADLINE, |data| {
        sheep_named(data, "greedy")["status"] == "online"
            && sheep_named(data, "unlimited")["status"] == "online"
    });
    assert_eq!(
        sheep_named(&before, "greedy")["restarts"],
        0,
        "precondition: {before}"
    );
    assert_eq!(
        sheep_named(&before, "unlimited")["restarts"],
        0,
        "precondition: {before}"
    );

    let after = poll_flock_data(home, BREACH_DEADLINE, |data| {
        sheep_named(data, "greedy")["restarts"] == 1
    });
    assert_eq!(
        sheep_named(&after, "greedy")["restarts"],
        1,
        "a process tree over its max_memory must be restarted by the real enforcer: {after}"
    );
    assert_eq!(
        sheep_named(&after, "unlimited")["restarts"],
        0,
        "the same script with no max_memory must not have moved: a restart both sheep \
         share is the script dying, not its ceiling being enforced: {after}"
    );

    // `launch.rs` redirects the daemon's stderr into this file, and the breach
    // record is the only place the observed RSS and its ceiling are stated.
    // Read rather than polled: `spawn_extras_reporter` writes it before asking
    // for the restart the counter above already saw.
    let daemon_log = std::fs::read_to_string(home.join("logs").join("shepd.err.log")).unwrap();
    assert!(
        daemon_log.contains("exceeded its max_memory"),
        "the daemon's own log must say why the sheep was restarted: {daemon_log:?}"
    );
    assert!(
        daemon_log.contains("limit="),
        "the record must carry the ceiling that was crossed: {daemon_log:?}"
    );

    graceful_kill(home);
}

// --- Case 16 -------------------------------------------------------------

#[cfg(unix)]
/// `SHEP_LOG_JSON=1` renders the daemon's own records as JSON, one object per
/// line, in the file `launch.rs` redirects its stderr into.
///
/// Every non-empty line is parsed, not only the one under test: a file where
/// one line in twenty is prose is not machine-readable.
#[test]
fn shep_log_json_makes_the_daemons_own_records_json() {
    let dir = tempfile::tempdir().unwrap();
    let log = daemon_log_after_a_missed_handshake(&dir, &[("SHEP_LOG_JSON", "1")]);

    let lines: Vec<&str> = log.lines().filter(|line| !line.trim().is_empty()).collect();
    assert!(
        !lines.is_empty(),
        "the daemon must have written something to read: {log:?}"
    );
    let records: Vec<serde_json::Value> = lines
        .iter()
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|err| {
                panic!("every line of shepd.err.log must be JSON under log_json: {line:?} ({err})")
            })
        })
        .collect();
    assert!(
        records.iter().any(|record| {
            record["level"] == "WARN"
                && record["fields"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains(READINESS_RECORD))
        }),
        "the readiness record must survive as a JSON object with its level and \
         message intact: {records:?}"
    );
}

// --- Case 17 -------------------------------------------------------------

#[cfg(unix)]
/// `tracing_subscriber` defaults to colour whenever its `ansi` feature is
/// compiled in, so `install_log_subscriber`'s `.with_ansi(ansi_enabled(..))`
/// is what keeps escapes out of `shepd.err.log`. Without it they land mid
/// field name and break every substring assertion in this file.
#[test]
fn the_daemons_own_log_carries_no_ansi_escapes() {
    let dir = tempfile::tempdir().unwrap();
    let log = daemon_log_after_a_missed_handshake(&dir, &[]);

    assert!(
        log.contains(READINESS_RECORD),
        "precondition: the daemon must have written a record to colour: {log:?}"
    );
    assert!(
        !log.contains('\x1b'),
        "a log file is not a terminal: {log:?}"
    );
}

#[cfg(unix)]
/// The same `WARN` record is written at the default level and filtered out at
/// `error`. Both halves provoke it on identical configuration, so the absent
/// half means filtered rather than never happened.
///
/// `error` rather than `off`: an `EnvFilter` built from an empty or
/// unparseable directive also degrades toward `off`, so silence alone would be
/// consistent with the level never being read.
#[test]
fn shep_log_level_decides_which_of_the_daemons_records_survive() {
    let at_default = tempfile::tempdir().unwrap();
    let default_log = daemon_log_after_a_missed_handshake(&at_default, &[]);
    assert!(
        default_log.contains(READINESS_RECORD),
        "a warn-level record must reach the log at the default level: {default_log:?}"
    );

    let at_error = tempfile::tempdir().unwrap();
    let error_log = daemon_log_after_a_missed_handshake(&at_error, &[("SHEP_LOG_LEVEL", "error")]);
    assert!(
        !error_log.contains(READINESS_RECORD),
        "SHEP_LOG_LEVEL=error must filter out the same warn-level record the \
         default level lets through: {error_log:?}"
    );
}

// --- Interpreter / spawn-failure parity -----------------------------------

#[cfg(unix)]
/// `Response::Restarted` has no per-id error slot, so a respawn that cannot
/// spawn answers `Ok` with an `errored` row rather than an RPC error;
/// `resume`'s `any_restart_failed` check is what closes that gap.
///
/// The script is valid shell but not executable (`0o644`), so every spawn of
/// it fails `EACCES` whichever request drove it.
#[test]
fn starting_an_errored_sheep_by_name_reports_the_same_failure_as_by_path() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("broken.sh");
    std::fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o644);
    std::fs::set_permissions(&script, perms).unwrap();
    let mut guard = DaemonGuard::default();

    // Also autostarts the daemon the second command reuses, and registers the
    // sheep the second half restarts by name.
    let by_path = shep(dir.path())
        .arg("--format")
        .arg("json")
        .arg("start")
        .arg(&script)
        .output()
        .unwrap();
    guard.adopt_home(dir.path());
    assert_json_error(&by_path, 7, "spawn_failed");

    // Must be `errored` in the flock, or the second command takes
    // `resolve_target`'s path arm instead of `resume`'s.
    let flock = poll_flock(dir.path(), |info| info["status"] == "errored");
    assert_eq!(
        flock["status"], "errored",
        "the by-path failure must leave the sheep registered as errored: {flock}"
    );

    // By name, same broken script, same failure.
    let name = script.file_stem().unwrap().to_str().unwrap();
    let by_name = shep(dir.path())
        .arg("--format")
        .arg("json")
        .arg("start")
        .arg(name)
        .output()
        .unwrap();
    assert_json_error(&by_name, 7, "spawn_failed");

    graceful_kill(dir.path());
}

#[cfg(unix)]
/// Same `Response::Restarted` gap as the sibling above. `restart` still prints
/// its table, being a multi-target verb; the exit code and the stderr line are
/// what change.
///
/// The script is valid shell at `0o644`, so every spawn fails `EACCES`, and it
/// has no extension: `.sh` maps to `sh` through the interpreter mapping, which
/// would run a non-executable file and delete the premise.
#[test]
fn restarting_a_sheep_that_cannot_spawn_reports_it_rather_than_exiting_zero() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("noexec");
    std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o644);
    std::fs::set_permissions(&script, perms).unwrap();
    let mut guard = DaemonGuard::default();

    let by_path = shep(dir.path())
        .arg("--format")
        .arg("json")
        .arg("start")
        .arg(&script)
        .output()
        .unwrap();
    guard.adopt_home(dir.path());
    assert_json_error(&by_path, 7, "spawn_failed");

    // Must be registered and errored, or the restart below is not
    // exercising the reply shape this test is about.
    let flock = poll_flock(dir.path(), |info| info["status"] == "errored");
    assert_eq!(
        flock["status"], "errored",
        "the by-path failure must leave the sheep registered as errored: {flock}"
    );

    let name = script.file_stem().unwrap().to_str().unwrap();
    let restarted = shep(dir.path())
        .arg("--format")
        .arg("json")
        .arg("restart")
        .arg(name)
        .output()
        .unwrap();
    assert_json_error(&restarted, 7, "spawn_failed");

    graceful_kill(dir.path());
}

#[cfg(unix)]
/// The missing-node sentence, produced for real rather than quoted.
///
/// It needs a `PATH` with no node on it, which a unit test could only get by
/// mutating its own process. `docs/migration.md` quotes this sentence, and
/// this is what holds the quote to the `format!` that produces it.
#[test]
fn a_js_flockfile_without_node_says_so_and_says_what_to_do() {
    let dir = tempfile::tempdir().unwrap();
    let flockfile = dir.path().join("Flockfile.js");
    // Declares a real app, so the only thing that can fail is the missing
    // interpreter: with node present this Flockfile is valid.
    std::fs::write(
        &flockfile,
        "module.exports = { app: [{ name: 'web', script: './server.js' }] };\n",
    )
    .unwrap();
    let mut guard = DaemonGuard::default();

    // An empty PATH for the child only, so `node` cannot be found and the
    // parent's environment is untouched.
    let output = shep(dir.path())
        .env("PATH", "")
        .arg("start")
        .arg("--flockfile")
        .arg(&flockfile)
        .output()
        .unwrap();

    // `start` autostarts a shepherd before it ever opens the Flockfile, so
    // this case leaves one behind even though it fails.
    guard.adopt_home(dir.path());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a Flockfile that cannot be read must not succeed: {stderr}"
    );
    assert!(
        stderr.contains("node was not found on PATH"),
        "the message names the cause: {stderr}"
    );
    assert!(
        stderr.contains("install node, or convert"),
        "and what to do about it: {stderr}"
    );
    assert!(
        !stderr.contains('\u{2014}') && !stderr.contains('\u{2013}'),
        "no em or en dash in copy a user reads: {stderr}"
    );

    graceful_kill(dir.path());
}

// --- shep init ---------------------------------------------------------------
//
// Writing a file is the behaviour under test, and a subprocess is the only
// place `shep init` runs.

#[test]
fn shep_init_writes_a_flockfile_where_there_is_none() {
    let dir = tempfile::tempdir().unwrap();

    let output = shep(dir.path())
        .current_dir(dir.path())
        .arg("init")
        .output()
        .unwrap();
    assert_success(&output);

    let written = dir.path().join("Flockfile.toml");
    assert!(written.exists(), "shep init must write Flockfile.toml");

    let body = std::fs::read_to_string(&written).unwrap();
    assert!(
        body.contains("[[app]]"),
        "the scaffold shows an app entry: {body}"
    );
    assert!(
        body.lines().any(|l| l.trim_start().starts_with('#')),
        "and it arrives commented out: {body}"
    );
}

#[cfg(unix)]
/// The unit tests prove the scaffold parses; this proves the bytes that reach
/// disk are the same ones.
#[test]
fn what_shep_init_writes_is_a_flockfile_shep_can_read() {
    let dir = tempfile::tempdir().unwrap();
    shep(dir.path())
        .current_dir(dir.path())
        .arg("init")
        .output()
        .unwrap();

    // Uncommenting is what makes it a live Flockfile: as written it declares
    // no apps and `shep start` refuses it.
    let body = std::fs::read_to_string(dir.path().join("Flockfile.toml")).unwrap();
    let live: String = body
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            match trimmed.strip_prefix('#') {
                Some(rest) if !rest.starts_with(' ') => rest.to_string(),
                _ => line.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(dir.path().join("Flockfile.toml"), &live).unwrap();
    let mut guard = DaemonGuard::default();

    let output = shep(dir.path())
        .current_dir(dir.path())
        .arg("--format")
        .arg("json")
        .arg("start")
        .arg("--flockfile")
        .arg("Flockfile.toml")
        .output()
        .unwrap();

    guard.adopt_home(dir.path());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("invalid_config"),
        "the uncommented scaffold must be valid config: {stderr}"
    );

    graceful_kill(dir.path());
}

#[cfg(unix)]
/// Proved by metadata, not content: a refusal that still rewrites the file
/// leaves identical bytes while the inode has changed and a symlinked config
/// has become a regular file.
#[test]
fn shep_init_refuses_an_existing_flockfile_without_touching_it() {
    let dir = tempfile::tempdir().unwrap();
    let existing = dir.path().join("Flockfile.toml");
    std::fs::write(
        &existing,
        "# mine\n[[app]]\nname = \"web\"\nscript = \"./s\"\n",
    )
    .unwrap();

    let before = std::fs::metadata(&existing).unwrap();

    let output = shep(dir.path())
        .current_dir(dir.path())
        .arg("init")
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "an existing Flockfile must not be overwritten silently"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Flockfile.toml"),
        "the refusal names the file: {stderr}"
    );

    let after = std::fs::metadata(&existing).unwrap();
    assert_eq!(
        before.ino(),
        after.ino(),
        "a refused write must not replace the file"
    );
    assert_eq!(
        before.permissions().mode(),
        after.permissions().mode(),
        "nor change its mode"
    );
    assert_eq!(
        std::fs::read_to_string(&existing).unwrap(),
        "# mine\n[[app]]\nname = \"web\"\nscript = \"./s\"\n",
        "nor its contents"
    );
}

#[test]
fn shep_init_force_replaces_an_existing_flockfile() {
    let dir = tempfile::tempdir().unwrap();
    let existing = dir.path().join("Flockfile.toml");
    std::fs::write(&existing, "# mine\n").unwrap();

    let output = shep(dir.path())
        .current_dir(dir.path())
        .arg("init")
        .arg("--force")
        .output()
        .unwrap();
    assert_success(&output);

    let body = std::fs::read_to_string(&existing).unwrap();
    assert!(
        body.contains("[[app]]"),
        "--force writes the scaffold over what was there: {body}"
    );
}

/// The depth flag reaches the file, not just the function.
#[test]
fn shep_init_all_writes_the_full_scaffold() {
    let dir = tempfile::tempdir().unwrap();

    let output = shep(dir.path())
        .current_dir(dir.path())
        .arg("init")
        .arg("--all")
        .output()
        .unwrap();
    assert_success(&output);

    let body = std::fs::read_to_string(dir.path().join("Flockfile.toml")).unwrap();
    for field in ["max_restarts", "kill_timeout", "watch_delay"] {
        assert!(
            body.contains(field),
            "--all names every option, and is missing `{field}`"
        );
    }
}

// --- Reload ---------------------------------------------------------------

#[cfg(unix)]
/// The envelope's `command` is what pins which handler `Commands::Reload`
/// reaches, since `main`'s dispatch arms have no unit coverage. The polled id
/// is the other half: a reload ends in a new id in the same instance slot,
/// where a restart would leave the id alone and a stop the sheep down.
///
/// The reply carries the original id, which is the acceptance contract:
/// `shep reload` exits before the swap commits, and the poll waits for it.
#[test]
fn reload_swaps_a_sheep_for_a_fresh_instance_under_a_new_id() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();

    let started = shep(dir.path())
        .arg("--format")
        .arg("json")
        .arg("start")
        .arg(&script)
        .output()
        .unwrap();
    guard.adopt_home(dir.path());
    assert_success(&started);
    let envelope: serde_json::Value = serde_json::from_slice(&started.stdout).unwrap();
    let original_id = envelope["data"][0]["id"]
        .as_u64()
        .unwrap_or_else(|| panic!("a started sheep must carry an id: {envelope}"));

    let reloaded = shep(dir.path())
        .arg("--format")
        .arg("json")
        .arg("reload")
        .arg("sheep")
        .output()
        .unwrap();
    assert_success(&reloaded);
    let envelope: serde_json::Value = serde_json::from_slice(&reloaded.stdout).unwrap();
    assert_eq!(
        envelope["command"], "reload",
        "`shep reload` must reach the reload verb and no other: {envelope}"
    );
    assert_eq!(
        envelope["data"][0]["id"], original_id,
        "the answer is the flock as it stood when the reload was accepted: {envelope}"
    );

    let after = poll_flock(dir.path(), |info| info["id"] != original_id);
    assert_ne!(
        after["id"], original_id,
        "the swap must finish, leaving one entry under a new id: {after}"
    );
    assert_eq!(after["status"], "online", "{after}");

    graceful_kill(dir.path());
}

// --- Trigger ---------------------------------------------------------------

#[cfg(unix)]
/// The envelope's `command` is what pins which handler `Commands::Trigger`
/// reaches, since `main`'s dispatch arms have no unit coverage.
///
/// The sheep has no `channel`/`wait_ready`/`shutdown_with_message`, so its
/// reply is `no_channel` every time without a companion that speaks the
/// shepherd channel. The other three outcomes need one and are not covered.
#[test]
fn trigger_reaches_the_trigger_verb_and_names_the_missing_channel() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();

    let started = shep(dir.path())
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("sheep")
        .output()
        .unwrap();
    guard.adopt_home(dir.path());
    assert_success(&started);

    let triggered = shep(dir.path())
        .arg("--format")
        .arg("json")
        .arg("trigger")
        .arg("sheep")
        .arg("reload-config")
        .output()
        .unwrap();
    assert_success(&triggered);
    let envelope: serde_json::Value = serde_json::from_slice(&triggered.stdout).unwrap();
    assert_eq!(
        envelope["command"], "trigger",
        "`shep trigger` must reach the trigger verb and no other: {envelope}"
    );
    assert_eq!(envelope["data"][0]["name"], "sheep", "{envelope}");
    assert_eq!(
        envelope["data"][0]["outcome"]["kind"], "no_channel",
        "a sheep with no channel/wait_ready/shutdown_with_message must answer \
         no_channel, never a reply it never opened a pipe to receive: {envelope}"
    );

    graceful_kill(dir.path());
}

// --- Signal ------------------------------------------------------------

#[cfg(unix)]
/// `SIGWINCH` is harmless to essentially everything, so the assertion is that
/// delivery reached the sheep, not what the child did with it.
#[test]
fn signal_reaches_the_signal_verb_and_delivers() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();

    let started = shep(dir.path())
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("sheep")
        .output()
        .unwrap();
    guard.adopt_home(dir.path());
    assert_success(&started);

    let signalled = shep(dir.path())
        .arg("--format")
        .arg("json")
        .arg("signal")
        .arg("sheep")
        .arg("SIGWINCH")
        .output()
        .unwrap();
    assert_success(&signalled);
    let envelope: serde_json::Value = serde_json::from_slice(&signalled.stdout).unwrap();
    assert_eq!(
        envelope["command"], "signal",
        "`shep signal` must reach the signal verb and no other: {envelope}"
    );
    assert_eq!(envelope["data"][0]["name"], "sheep", "{envelope}");
    assert_eq!(
        envelope["data"][0]["outcome"]["kind"], "delivered",
        "a running sheep must answer delivered for a signal the kernel accepted: {envelope}"
    );

    graceful_kill(dir.path());
}

// --- Stock -------------------------------------------------------------

#[cfg(unix)]
/// Both directions are polled through `shep flock` rather than taken off
/// `stock`'s own exit: a stock-down accepts before the departing instances'
/// stop ladders finish, so the flock settling is the real assertion.
#[test]
fn stock_reaches_the_stock_verb_and_settles_the_flock() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();

    let started = shep(dir.path())
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("sheep")
        .output()
        .unwrap();
    guard.adopt_home(dir.path());
    assert_success(&started);

    let stocked_up = shep(dir.path())
        .arg("--format")
        .arg("json")
        .arg("stock")
        .arg("sheep")
        .arg("3")
        .output()
        .unwrap();
    assert_success(&stocked_up);
    let envelope: serde_json::Value = serde_json::from_slice(&stocked_up.stdout).unwrap();
    assert_eq!(
        envelope["command"], "stock",
        "`shep stock` must reach the stock verb and no other: {envelope}"
    );

    let grown = poll_flock_data(dir.path(), FLOCK_DEADLINE, |data| {
        data.as_array().is_some_and(|rows| rows.len() == 3)
    });
    assert_eq!(
        grown.as_array().unwrap().len(),
        3,
        "stocking up must settle at three instances: {grown}"
    );
    assert!(
        grown
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["name"] == "sheep"),
        "every instance must still belong to `sheep`: {grown}"
    );

    let stocked_down = shep(dir.path())
        .arg("stock")
        .arg("sheep")
        .arg("1")
        .output()
        .unwrap();
    assert_success(&stocked_down);

    let settled = poll_flock_data(dir.path(), FLOCK_DEADLINE, |data| {
        data.as_array().is_some_and(|rows| rows.len() == 1)
    });
    assert_eq!(
        settled.as_array().unwrap().len(),
        1,
        "stocking down must settle back to one instance: {settled}"
    );

    graceful_kill(dir.path());
}

#[cfg(unix)]
/// `shep scale` is `stock`'s visible alias, and must produce the same primary
/// command name in its envelope.
#[test]
fn scale_alias_reaches_stock_against_a_real_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();

    let started = shep(dir.path())
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("sheep")
        .output()
        .unwrap();
    guard.adopt_home(dir.path());
    assert_success(&started);

    let scaled = shep(dir.path())
        .arg("--format")
        .arg("json")
        .arg("scale")
        .arg("sheep")
        .arg("2")
        .output()
        .unwrap();
    assert_success(&scaled);
    let envelope: serde_json::Value = serde_json::from_slice(&scaled.stdout).unwrap();
    assert_eq!(
        envelope["command"], "stock",
        "`shep scale` is an alias for `stock`, and must reach it: {envelope}"
    );

    graceful_kill(dir.path());
}

// --- Lambs ---------------------------------------------------------------

#[cfg(unix)]
/// Polled, not asserted once: the daemon walks lambs inside `Describe` against
/// the live process table, so the forked child's appearance races this test.
#[test]
fn describe_renders_a_real_sheeps_lamb_tree() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_forking_script(&dir);
    let mut guard = DaemonGuard::default();

    let started = shep(dir.path())
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("sheep")
        .output()
        .unwrap();
    guard.adopt_home(dir.path());
    assert_success(&started);

    // Polls for `sleep`, not merely a `Lambs of` section: a walk can catch the
    // child mid-exec still reporting the shell's name. It rides that out only
    // because `MemorySampler::identify` builds a process table per call;
    // sysinfo never revises a name it has recorded for a pid.
    let start = Instant::now();
    let described = loop {
        let output = shep(dir.path())
            .arg("describe")
            .arg("sheep")
            .output()
            .unwrap();
        assert_success(&output);
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        if text.contains("sleep") || start.elapsed() >= FLOCK_DEADLINE {
            break text;
        }
        std::thread::sleep(FLOCK_POLL_INTERVAL);
    };

    assert!(described.contains("Lambs of"), "{described}");
    assert!(described.contains("sleep"), "{described}");
    assert!(
        described.contains("not exactly the set a stop kills"),
        "{described}"
    );

    graceful_kill(dir.path());
}

// --- Save / Muster ---------------------------------------------------------

#[cfg(unix)]
/// Nothing goes down in between, so the muster exercises the already-running
/// idempotence rule in `snapshot::restorable`.
///
/// `flock.len()` pins exactly one instance and `pid` pins it as the process
/// `start` reported, so a muster that duplicated or restarted the sheep fails
/// here.
#[test]
fn saving_the_roll_then_mustering_reports_the_same_flock() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();

    let started = shep(home)
        .arg("--format")
        .arg("json")
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("roundtrip")
        .output()
        .unwrap();
    guard.adopt_home(home);
    assert_success(&started);
    let start_envelope: serde_json::Value = serde_json::from_slice(&started.stdout).unwrap();
    assert_eq!(
        start_envelope["data"][0]["status"], "online",
        "{start_envelope}"
    );
    let original_pid = start_envelope["data"][0]["pid"]
        .as_i64()
        .unwrap_or_else(|| panic!("pid must be a real positive OS pid: {start_envelope}"));

    let saved = shep(home)
        .arg("--format")
        .arg("json")
        .arg("save")
        .output()
        .unwrap();
    assert_success(&saved);
    let save_envelope: serde_json::Value = serde_json::from_slice(&saved.stdout).unwrap();
    assert_eq!(
        save_envelope["command"], "save",
        "`shep save` must reach the save verb and no other: {save_envelope}"
    );
    assert_eq!(
        save_envelope["data"]["apps"], 1,
        "the roll must record the one app started above: {save_envelope}"
    );

    let mustered = shep(home)
        .arg("--format")
        .arg("json")
        .arg("muster")
        .output()
        .unwrap();
    assert_success(&mustered);
    let muster_envelope: serde_json::Value = serde_json::from_slice(&mustered.stdout).unwrap();
    assert_eq!(
        muster_envelope["command"], "muster",
        "`shep muster` must reach the muster verb and no other: {muster_envelope}"
    );
    let flock = muster_envelope["data"]
        .as_array()
        .unwrap_or_else(|| panic!("muster data must be an array: {muster_envelope}"));
    assert_eq!(
        flock.len(),
        1,
        "muster against a daemon already running the flock the roll \
         describes must not spawn a duplicate: {muster_envelope}"
    );
    assert_eq!(flock[0]["name"], "roundtrip", "{muster_envelope}");
    assert_eq!(
        flock[0]["pid"].as_i64().unwrap(),
        original_pid,
        "muster must leave an already-running sheep alone and report the \
         SAME process, never restart it: {muster_envelope}"
    );

    graceful_kill(home);
}

// --- Import -----------------------------------------------------------

#[cfg(unix)]
/// The written file is parsed back through the real
/// `shep_core::config::Flockfile::parse`: a Flockfile shep refuses to read is
/// not an import. That no socket appears is the other half, since `import`
/// takes no `Client`.
#[test]
fn import_writes_a_flockfile_shep_can_read_back_and_starts_no_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let out = home.join("Flockfile.toml");
    let dump = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/commands/import/testdata/dump.pm2.json"
    );
    let mut guard = DaemonGuard::default();

    let output = shep(home)
        .arg("--format")
        .arg("json")
        .arg("import")
        .arg("--from")
        .arg(dump)
        .arg("--out")
        .arg(&out)
        .output()
        .unwrap();
    guard.adopt_home(home);
    assert_success(&output);

    assert!(
        !home.join("run").join("shep.sock").exists(),
        "`shep import` reads a file and writes a file; it must never \
         autostart a daemon"
    );

    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        envelope["command"], "import",
        "`shep import` must reach the import verb and no other: {envelope}"
    );
    let rows = envelope["data"]
        .as_array()
        .unwrap_or_else(|| panic!("import data must be an array: {envelope}"));
    assert_eq!(rows.len(), 3, "{envelope}");

    let written = std::fs::read_to_string(&out).unwrap();
    let parsed =
        shep_core::config::Flockfile::parse(&written, shep_core::config::FlockFormat::Toml)
            .unwrap_or_else(|e| {
                panic!("shep import wrote a Flockfile shep cannot read back: {e}\n{written}")
            });
    assert_eq!(parsed.apps.len(), 3, "{written}");
}

// --- Dogs / Barks -----------------------------------------------------

/// Writes `$SHEP_HOME/shep.toml` directly, before any daemon boots off it.
/// Neither `shep enable` nor `shep adopt` has a flag for `[dog.metrics] bind`,
/// which every case below needs to avoid colliding with a real `9615`.
fn write_shep_toml(dir: &TempDir, body: &str) -> PathBuf {
    let path = dir.path().join("shep.toml");
    std::fs::write(&path, body).unwrap();
    path
}

#[cfg(unix)]
/// The only tier that exec's `shep dog metrics`: every other scripts the
/// runner or fakes the client.
///
/// Four things fail here as a refused connection: the dog being spawned, its
/// reaching the socket from `$SHEP_HOME`, its fetching its own
/// `[dog.metrics]` section, and its bind.
#[test]
fn a_real_shepherd_runs_a_real_metrics_dog_that_answers_a_scrape() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let port = free_port();
    write_shep_toml(
        &dir,
        &format!("[dog.metrics]\nbind = \"127.0.0.1:{port}\"\n"),
    );
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();

    let started = shep(home)
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("web")
        .output()
        .unwrap();
    guard.adopt_home(home);
    assert_success(&started);

    let online = poll_flock(home, |info| info["status"] == "online");
    assert_eq!(
        online["status"], "online",
        "the sheep must reach online before the dog's own exposition has \
         anything real to name: {online}"
    );

    let enabled = shep(home).arg("enable").arg("metrics").output().unwrap();
    assert_success(&enabled);

    // Registered before the scrape: one that hangs or panics on an assertion
    // must not leak the grandchild the daemon just spawned.
    let dog_pid = wait_for_dog_pid(home, "metrics");
    guard.adopt_dog_pid(dog_pid);

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let body = poll_metrics(addr);
    assert!(
        body.contains("HTTP/1.1 200"),
        "the metrics dog must answer 200 at /metrics: {body}"
    );
    assert!(
        body.contains(r#"shep_sheep_status{sheep="web",id="0",fold="",status="online"} 1"#),
        "the exposition must name the sheep, online: {body}"
    );
    assert!(
        body.contains(r#"shep_dog_up{dog="metrics",source="built-in"} 1"#),
        "the dog must report itself up while it is the one serving the \
         scrape that says so: {body}"
    );

    graceful_kill(home);
}

#[cfg(unix)]
/// [`poll_metrics`], retried until the exposition contains `needle` rather
/// than merely until it answers: a dog answering from a cached reading still
/// answers 200, so only content the predecessor never saw tells them apart.
fn poll_metrics_containing(addr: std::net::SocketAddr, needle: &str) -> String {
    let start = Instant::now();
    let mut last = String::new();
    loop {
        if let Ok(body) = scrape_metrics(addr) {
            if body.contains(needle) {
                return body;
            }
            last = body;
        }
        if start.elapsed() >= METRICS_SCRAPE_DEADLINE {
            return last;
        }
        std::thread::sleep(METRICS_SCRAPE_POLL_INTERVAL);
    }
}

#[cfg(unix)]
/// A real dog process, carried across a real `execve`, still able to talk to
/// the shepherd that replaced the one it handshook with.
///
/// A pid check cannot see the defect: a dog holding a dead socket reads as
/// healthy on every column a listing has. The decisive assertion is content, a
/// sheep started after the reload appearing in the exposition.
#[test]
fn a_carried_dog_answers_a_scrape_after_a_real_reload() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let port = free_port();
    write_shep_toml(
        &dir,
        &format!("[dog.metrics]\nbind = \"127.0.0.1:{port}\"\n"),
    );
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();

    let started = shep(home)
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("web")
        .output()
        .unwrap();
    guard.adopt_home(home);
    assert_success(&started);
    let online = poll_flock(home, |info| info["status"] == "online");
    let sheep_pid = online["pid"].as_u64().expect("an online sheep has a pid");

    let enabled = shep(home).arg("enable").arg("metrics").output().unwrap();
    assert_success(&enabled);
    let dog_pid = wait_for_dog_pid(home, "metrics");
    guard.adopt_dog_pid(dog_pid);

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let before = poll_metrics(addr);
    assert!(
        before.contains("HTTP/1.1 200"),
        "the dog must be answering BEFORE the reload, or this case proves nothing: {before}"
    );

    let reloaded = shep(home).arg("daemon").arg("reload").output().unwrap();
    assert_success(&reloaded);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&reloaded.stdout),
        String::from_utf8_lossy(&reloaded.stderr)
    );
    // The exact sentence `handover::RefusedReason`'s `Display` ends with: the
    // stop arm restarts every dog from disk and would satisfy a looser probe.
    assert!(
        !text.contains("falls back to a stop-and-start"),
        "a flock with a dog in it is carried now, not refused: {text}"
    );
    assert!(
        !text.contains("cannot talk to this shepherd"),
        "the carried dog must reconnect rather than be reported stale: {text}"
    );
    assert!(
        !text.contains("cannot say whether it came back"),
        "the carried dog must answer inside the reload's own wait: {text}"
    );

    let after = poll_flock_data(home, FLOCK_DEADLINE, |data| {
        data.as_array()
            .is_some_and(|rows| rows.len() == 2 && rows.iter().all(|row| !row["pid"].is_null()))
    });
    let rows = after.as_array().expect("flock data is an array");
    let dog_row = rows
        .iter()
        .find(|row| row["name"] == "metrics")
        .unwrap_or_else(|| panic!("the dog must still be registered: {after}"));
    let sheep_row = rows
        .iter()
        .find(|row| row["name"] == "web")
        .unwrap_or_else(|| panic!("the sheep must still be registered: {after}"));

    assert_eq!(
        dog_row["pid"].as_u64(),
        Some(u64::try_from(dog_pid.as_raw()).unwrap()),
        "the dog was restarted rather than carried: {after}"
    );
    assert_eq!(
        sheep_row["pid"].as_u64(),
        Some(sheep_pid),
        "the sheep was restarted rather than carried: {after}"
    );
    assert_eq!(
        dog_row["restarts"], 0,
        "the dog's restart count moved: {after}"
    );
    assert_eq!(
        sheep_row["restarts"], 0,
        "the sheep's restart count moved: {after}"
    );
    // JSON carries both populations in one undivided array, so the marker is
    // what keeps them apart here; the tables below are what an operator sees.
    assert_eq!(
        dog_row["dog"]["kind"], "built_in",
        "a carried dog that lost its marker is one `shep dogs` has lost: {after}"
    );
    assert!(
        sheep_row["dog"].is_null(),
        "a sheep must not pick a marker up on the way across: {after}"
    );

    let dogs = shep(home).arg("dogs").output().unwrap();
    assert_success(&dogs);
    assert!(
        String::from_utf8_lossy(&dogs.stdout).contains("metrics"),
        "`shep dogs` must still list the carried dog: {}",
        String::from_utf8_lossy(&dogs.stdout)
    );
    let flock = shep(home).arg("flock").output().unwrap();
    assert_success(&flock);
    let flock_text = String::from_utf8_lossy(&flock.stdout);
    let (sheep_table, _dogs_table) = flock_text
        .split_once("Dogs")
        .unwrap_or_else(|| panic!("`shep flock` prints a dogs section: {flock_text}"));
    assert!(
        !sheep_table.contains("metrics"),
        "a carried dog must not be listed beside the operator's own apps: {flock_text}"
    );

    // The decisive one. A sheep that did not exist when the predecessor was
    // running, named by the dog that is answering now.
    let fresh = shep(home)
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("freshsheep")
        .output()
        .unwrap();
    assert_success(&fresh);
    let body = poll_metrics_containing(addr, r#"sheep="freshsheep""#);
    assert!(
        body.contains("HTTP/1.1 200"),
        "the carried dog must still answer a scrape after the exec: {body}"
    );
    assert!(
        body.contains(r#"sheep="freshsheep""#),
        "the exposition must name a sheep started AFTER the reload, which no cached reading \
         and no connection to the predecessor could produce: {body}"
    );

    // A successor rebuilds the roll from the blob, not from disk.
    // `spawn_enabled_dogs` never touches `FlockRegistry`, so a dog in the roll
    // would outlive the daemon and a later cold boot would restore `metrics`
    // as an ordinary unmarked sheep.
    let saved = shep(home)
        .arg("--format")
        .arg("json")
        .arg("save")
        .output()
        .unwrap();
    assert_success(&saved);
    let roll: serde_json::Value = serde_json::from_slice(&saved.stdout).unwrap();
    assert_eq!(
        roll["data"]["apps"], 2,
        "the roll holds the two sheep and no dog: {roll}"
    );

    graceful_kill(home);
}

#[cfg(unix)]
/// Table format, not JSON: `Format::Json`'s `flock` answer carries both
/// populations in one undivided array.
#[test]
fn dogs_and_flock_render_the_two_populations_the_right_way_round() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let port = free_port();
    write_shep_toml(
        &dir,
        &format!("[dog.metrics]\nbind = \"127.0.0.1:{port}\"\n"),
    );
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();

    let started = shep(home)
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("web")
        .output()
        .unwrap();
    guard.adopt_home(home);
    assert_success(&started);
    poll_flock(home, |info| info["status"] == "online");

    let enabled = shep(home).arg("enable").arg("metrics").output().unwrap();
    assert_success(&enabled);
    guard.adopt_dog_pid(wait_for_dog_pid(home, "metrics"));

    let flock_table = String::from_utf8(shep(home).arg("flock").output().unwrap().stdout).unwrap();
    assert!(
        flock_table.contains("web"),
        "shep flock must still render the sheep: {flock_table}"
    );
    assert!(
        flock_table.contains("Dogs") && flock_table.contains("metrics"),
        "shep flock must render the dogs section beneath the sheep table: {flock_table}"
    );

    let dogs_table = String::from_utf8(shep(home).arg("dogs").output().unwrap().stdout).unwrap();
    assert!(
        dogs_table.contains("metrics"),
        "shep dogs must render the dog: {dogs_table}"
    );
    assert!(
        !dogs_table.contains("web"),
        "shep dogs must render nothing but dogs — not the sheep: {dogs_table}"
    );
    assert!(
        !dogs_table.contains("Dogs\n"),
        "shep dogs must not carry flock's own section header — it IS the \
         dogs table, not a listing with one embedded: {dogs_table}"
    );

    graceful_kill(home);
}

#[test]
fn barks_reads_the_history_with_no_shepherd_running() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let bark = shep_core::barks::Bark {
        at_ms: 1_700_000_000_000,
        rule: "watchdog".to_string(),
        subject: "web".to_string(),
        message: "restart budget exhausted".to_string(),
        sinks: vec![shep_core::barks::SinkOutcome {
            sink: "ops".to_string(),
            error: None,
        }],
    };
    shep_core::barks::append(
        &home.join("barks.jsonl"),
        &bark,
        shep_core::barks::DEFAULT_MAX_BYTES,
    )
    .unwrap();
    assert!(
        !home.join("run").join("shep.sock").exists(),
        "this case never starts a daemon at all"
    );

    let output = shep(home)
        .arg("--format")
        .arg("json")
        .arg("barks")
        .output()
        .unwrap();
    assert_success(&output);
    assert!(
        !home.join("run").join("shep.sock").exists(),
        "`shep barks` must never autostart a shepherd either"
    );

    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        envelope["command"], "barks",
        "`shep barks` must reach the barks verb and no other: {envelope}"
    );
    let rows = envelope["data"]
        .as_array()
        .unwrap_or_else(|| panic!("barks data must be an array: {envelope}"));
    assert_eq!(rows.len(), 1, "{envelope}");
    assert_eq!(rows[0]["subject"], "web", "{envelope}");
    assert_eq!(rows[0]["rule"], "watchdog", "{envelope}");
}

#[cfg(unix)]
/// The whole store through the real binary, with no shepherd anywhere:
/// provisioning happens when nothing is running. Also checks the `0600` mode
/// `shep_core::kv` documents, on the store the first `set` creates.
#[test]
fn the_kv_store_works_with_no_shepherd_running() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();

    let set1 = shep(home)
        .arg("set")
        .arg("bark.cooldown")
        .arg("30s")
        .output()
        .unwrap();
    assert_success(&set1);
    assert!(
        !home.join("run").join("shep.sock").exists(),
        "shep set must never autostart a shepherd"
    );

    let mode = std::fs::metadata(home.join("kv.json"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "{mode:o}");

    let get1 = shep(home).arg("get").arg("bark.cooldown").output().unwrap();
    assert_success(&get1);
    assert!(
        String::from_utf8_lossy(&get1.stdout).contains("30s"),
        "{}",
        String::from_utf8_lossy(&get1.stdout)
    );

    let missing = shep(home).arg("get").arg("missing").output().unwrap();
    assert_eq!(missing.status.code(), Some(3), "NotFound; {missing:?}");

    let set2 = shep(home)
        .arg("set")
        .arg("metrics_port")
        .arg("9615")
        .output()
        .unwrap();
    assert_success(&set2);

    let both = shep(home).arg("get").output().unwrap();
    assert_success(&both);
    let both_text = String::from_utf8_lossy(&both.stdout);
    assert!(both_text.contains("bark.cooldown"), "{both_text}");
    assert!(both_text.contains("metrics_port"), "{both_text}");

    let unset1 = shep(home)
        .arg("unset")
        .arg("bark.cooldown")
        .output()
        .unwrap();
    assert_success(&unset1);

    let gone = shep(home).arg("get").arg("bark.cooldown").output().unwrap();
    assert_eq!(gone.status.code(), Some(3), "NotFound; {gone:?}");

    let unset_all = shep(home).arg("unset").arg("--all").output().unwrap();
    assert_success(&unset_all);

    let empty = shep(home).arg("get").output().unwrap();
    assert_success(&empty);
    let empty_text = String::from_utf8_lossy(&empty.stdout);
    assert!(
        !empty_text.contains("metrics_port"),
        "store must be empty after unset --all: {empty_text}"
    );

    let bad_key = shep(home)
        .arg("set")
        .arg("bad key")
        .arg("x")
        .output()
        .unwrap();
    assert_eq!(bad_key.status.code(), Some(2), "usage; {bad_key:?}");
}

/// `data` is an array of `{key, value}` objects, never a JSON map keyed by
/// name. An absent key and a key outside the grammar surface as this
/// envelope's error half.
#[test]
fn kv_json_envelope_is_an_array_with_the_schema_version() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();

    shep(home).arg("set").arg("a").arg("1").output().unwrap();
    shep(home).arg("set").arg("b").arg("2").output().unwrap();

    let get_all = shep(home)
        .arg("--format")
        .arg("json")
        .arg("get")
        .output()
        .unwrap();
    assert_success(&get_all);
    let envelope: serde_json::Value = serde_json::from_slice(&get_all.stdout).unwrap();
    assert!(envelope["data"].is_array(), "{envelope}");
    assert_eq!(envelope["data"].as_array().unwrap().len(), 2, "{envelope}");
    assert_eq!(envelope["schema_version"], 1, "{envelope}");

    let missing = shep(home)
        .arg("--format")
        .arg("json")
        .arg("get")
        .arg("ghost")
        .output()
        .unwrap();
    assert_json_error(&missing, 3, "not_found");

    let bad_key = shep(home)
        .arg("--format")
        .arg("json")
        .arg("set")
        .arg("bad key")
        .arg("x")
        .output()
        .unwrap();
    assert_json_error(&bad_key, 2, "usage");
}

/// Two real processes, not two threads sharing one open-file-description
/// table: only separate processes contend for `kv.json.lock`'s `flock(2)`.
/// The barrier matters: without it one writer can finish its whole batch
/// before the other starts, racing nothing.
#[test]
fn two_real_shep_processes_writing_concurrently_lose_no_keys() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    const PER_WRITER: usize = 15;

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let (finished, racers) = std::sync::mpsc::channel();
    for writer in 0..2 {
        let home = home.clone();
        let barrier = std::sync::Arc::clone(&barrier);
        let finished = finished.clone();
        std::thread::spawn(move || {
            barrier.wait(); // both writers start their first `shep set` together
            for n in 0..PER_WRITER {
                let key = format!("writer{writer}.k{n}");
                let output = shep(&home).arg("set").arg(&key).arg("v").output().unwrap();
                // A closed receiver means the case already gave up on this
                // writer and failed; there is no one left to report to.
                let _ = finished.send((writer, key, output));
            }
        });
    }
    drop(finished); // the writer threads hold the only senders that matter

    for _ in 0..(PER_WRITER * 2) {
        let (writer, key, output) = racers
            .recv_timeout(RACER_DEADLINE)
            .expect("a writer never came back; see RACER_DEADLINE");
        assert!(
            output.status.success(),
            "writer {writer}, key {key}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let list = shep(&home)
        .arg("--format")
        .arg("json")
        .arg("get")
        .output()
        .unwrap();
    assert_success(&list);
    let envelope: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    let data = envelope["data"]
        .as_array()
        .unwrap_or_else(|| panic!("get data must be an array: {envelope}"));
    assert_eq!(
        data.len(),
        PER_WRITER * 2,
        "two concurrent shep set processes must not lose each other's keys: {envelope}"
    );
}

/// `assert_cmd` captures stdout through a pipe, so this is the not-a-tty
/// refusal a `shep lookout > dash.txt` meets.
#[test]
fn shep_lookout_refuses_when_stdout_is_not_a_terminal() {
    let home = TempDir::new().unwrap();
    let output = shep(home.path())
        .arg("lookout")
        .timeout(CMD_TIMEOUT)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("needs a terminal"));
}

#[test]
fn shep_dash_is_the_same_verb() {
    let home = TempDir::new().unwrap();
    let output = shep(home.path())
        .arg("dash")
        .timeout(CMD_TIMEOUT)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("needs a terminal")
    );
}

/// The assertion is on `security boundary` alone: `wrap_help` re-wraps long
/// help at the detected terminal width, so a longer phrase can land across a
/// line break on one machine and not another.
#[test]
fn shep_lookout_help_names_the_gate() {
    let home = TempDir::new().unwrap();
    let output = shep(home.path())
        .args(["lookout", "--help"])
        .timeout(CMD_TIMEOUT)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("--allow-control"));
    assert!(text.contains("security boundary"));
}

// ---------------------------------------------------------------------------
// whistle: the MCP interface, driven over real pipes.
// ---------------------------------------------------------------------------

/// Serializes `value` as compact JSON followed by `\n`, the newline-delimited
/// framing `transport-io`'s codec expects on both sides of the pipe.
fn push_mcp_line(buf: &mut Vec<u8>, value: &serde_json::Value) {
    buf.extend_from_slice(value.to_string().as_bytes());
    buf.push(b'\n');
}

/// Stdin for one MCP session: the `initialize` handshake (id `1`), the
/// `notifications/initialized`, then each of `requests`. `"2025-06-18"` is a
/// `ProtocolVersion::KNOWN_VERSIONS` entry rather than `LATEST`, so an rmcp
/// bump does not redden this suite.
fn mcp_session(requests: &[serde_json::Value]) -> Vec<u8> {
    let mut buf = Vec::new();
    push_mcp_line(
        &mut buf,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "cli_e2e", "version": "0.0.0"},
            },
        }),
    );
    push_mcp_line(
        &mut buf,
        &serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );
    for request in requests {
        push_mcp_line(&mut buf, request);
    }
    buf
}

/// A `tools/list` request with the given id.
fn tools_list_request(id: i64) -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "tools/list"})
}

/// A `tools/call` request. `arguments` is omitted rather than sent as `{}`
/// when a tool takes none, matching what a real client sends.
fn call_tool_request(
    id: i64,
    name: &str,
    arguments: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut params = serde_json::json!({"name": name});
    if let Some(args) = arguments {
        params
            .as_object_mut()
            .expect("params is always an object")
            .insert("arguments".to_string(), args);
    }
    serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params})
}

/// Parses every line of `stdout` as JSON-RPC, panicking with the offending
/// line otherwise. A search for the wanted reply alone would pass with a stray
/// `println!` or a tracing record on the same wire. `str::lines` yields no
/// trailing empty entry, so an empty line is one the verb wrote.
fn assert_every_stdout_line_is_jsonrpc(stdout: &[u8]) -> Vec<serde_json::Value> {
    let text = String::from_utf8(stdout.to_vec()).expect("whistle's stdout is valid UTF-8");
    text.lines()
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|err| panic!("stdout line is not JSON: {err}\nline: {line}"));
            assert_eq!(
                value.get("jsonrpc").and_then(serde_json::Value::as_str),
                Some("2.0"),
                "stdout line is not JSON-RPC: {line}"
            );
            value
        })
        .collect()
}

/// The reply among `lines` whose `"id"` matches, told apart from a request or
/// notification of the same shape by carrying `"result"` or `"error"`.
fn find_reply(lines: &[serde_json::Value], id: i64) -> &serde_json::Value {
    lines
        .iter()
        .find(|line| {
            line.get("id") == Some(&serde_json::Value::from(id))
                && (line.get("result").is_some() || line.get("error").is_some())
        })
        .unwrap_or_else(|| panic!("no reply with id {id} in {lines:#?}"))
}

/// A `shep` invocation reaching `$SHEP_HOME` through the environment rather
/// than `--home`; `GlobalArgs::home` carries `env = "SHEP_HOME"`.
fn shep_via_env(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("shep").unwrap();
    cmd.env("SHEP_HOME", home).timeout(CMD_TIMEOUT);
    cmd
}

/// Drives `cmd` (already carrying `--home` or `SHEP_HOME`, not yet the
/// `whistle` argument) through an `initialize` handshake and a
/// `tools/list`, and returns the tool names the gate produced.
fn whistle_tool_names(mut cmd: Command) -> Vec<String> {
    let stdin = mcp_session(&[tools_list_request(2)]);
    let output = cmd.arg("whistle").write_stdin(stdin).output().unwrap();
    assert_success(&output);
    let lines = assert_every_stdout_line_is_jsonrpc(&output.stdout);
    find_reply(&lines, 2)["result"]["tools"]
        .as_array()
        .expect("tools/list result carries a tools array")
        .iter()
        .map(|tool| {
            tool["name"]
                .as_str()
                .expect("every tool has a name")
                .to_string()
        })
        .collect()
}

/// Drives the real binary: an `initialize` and a `tools/list` request,
/// newline-delimited on stdin, replies read back from stdout. Every stdout
/// line must parse as JSON-RPC.
#[test]
fn whistle_speaks_mcp_and_writes_nothing_else_to_stdout() {
    let home = TempDir::new().unwrap();
    let stdin = mcp_session(&[tools_list_request(2)]);
    let output = shep(home.path())
        .arg("whistle")
        .write_stdin(stdin)
        .output()
        .unwrap();
    assert_success(&output);

    let lines = assert_every_stdout_line_is_jsonrpc(&output.stdout);

    let init_reply = find_reply(&lines, 1);
    assert_eq!(init_reply["result"]["serverInfo"]["name"], "shep");
    assert!(init_reply["result"]["capabilities"]["tools"].is_object());

    let list_reply = find_reply(&lines, 2);
    assert!(list_reply["result"]["tools"].is_array());
}

/// Three runs against two `$SHEP_HOME`s: no `[whistle]` section (five tools),
/// `allow_control = true` (nine), and that same open directory again through
/// `--home`. The split is checked by name, not only by count: a count alone
/// would pass if the gate registered a read tool twice.
#[test]
fn the_shep_toml_gate_decides_the_tool_list_in_a_real_process() {
    let control_tools = ["start_sheep", "stop_sheep", "restart_sheep", "reload_sheep"];

    let closed_home = TempDir::new().unwrap();
    let names = whistle_tool_names(shep_via_env(closed_home.path()));
    assert_eq!(names.len(), 5, "read-only: {names:?}");
    for tool in control_tools {
        assert!(
            !names.contains(&tool.to_string()),
            "{tool} must be absent: {names:?}"
        );
    }

    let open_home = TempDir::new().unwrap();
    write_shep_toml(&open_home, "[whistle]\nallow_control = true\n");

    let names = whistle_tool_names(shep_via_env(open_home.path()));
    assert_eq!(names.len(), 9, "gate open via env: {names:?}");
    for tool in control_tools {
        assert!(
            names.contains(&tool.to_string()),
            "{tool} must be present: {names:?}"
        );
    }

    let names = whistle_tool_names(shep(open_home.path()));
    assert_eq!(names.len(), 9, "gate open via --home: {names:?}");
    for tool in control_tools {
        assert!(
            names.contains(&tool.to_string()),
            "{tool} must be present: {names:?}"
        );
    }
}

/// The malformed-config notice is the only thing whistle writes outside the
/// JSON-RPC wire, and it sits next to the stdout handle. A config that fails
/// to parse leaves the gate shut.
#[test]
fn a_malformed_shep_toml_stays_off_stdout_and_keeps_the_gate_shut() {
    let home = TempDir::new().unwrap();
    write_shep_toml(&home, "[whistle\n");

    let stdin = mcp_session(&[tools_list_request(2)]);
    let output = shep(home.path())
        .arg("whistle")
        .write_stdin(stdin)
        .output()
        .unwrap();
    assert_success(&output);

    let lines = assert_every_stdout_line_is_jsonrpc(&output.stdout);
    let list_reply = find_reply(&lines, 2);
    let names: Vec<String> = list_reply["result"]["tools"]
        .as_array()
        .expect("tools/list result carries a tools array")
        .iter()
        .map(|tool| {
            tool["name"]
                .as_str()
                .expect("every tool has a name")
                .to_string()
        })
        .collect();

    assert_eq!(
        names.len(),
        5,
        "a broken config must read as the gate SHUT, not open: {names:?}"
    );
    for tool in ["start_sheep", "stop_sheep", "restart_sheep", "reload_sheep"] {
        assert!(
            !names.contains(&tool.to_string()),
            "{tool} must be absent when shep.toml fails to parse: {names:?}"
        );
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid_config"),
        "the malformed-config notice must reach stderr: {stderr}"
    );
    assert!(
        stderr.contains("shep.toml"),
        "the notice must name the file: {stderr}"
    );
}

/// With the gate shut, `tools/call` for `stop_sheep` answers JSON-RPC error
/// `-32602`, rmcp's answer for a name its router does not hold. A tool that
/// existed and refused would answer a `result`.
#[test]
fn a_gated_off_control_tool_is_not_merely_refused_it_is_absent() {
    let home = TempDir::new().unwrap();
    let stdin = mcp_session(&[call_tool_request(
        2,
        "stop_sheep",
        Some(serde_json::json!({"name": "api"})),
    )]);
    let output = shep(home.path())
        .arg("whistle")
        .write_stdin(stdin)
        .output()
        .unwrap();
    assert_success(&output);

    let lines = assert_every_stdout_line_is_jsonrpc(&output.stdout);
    let reply = find_reply(&lines, 2);
    assert!(
        reply.get("result").is_none(),
        "a gated-off tool must be a protocol error, not a result: {reply:#?}"
    );
    let error = reply
        .get("error")
        .expect("a gated-off tool call must answer a JSON-RPC error");
    assert_eq!(error["code"], -32602);
    assert_eq!(error["message"], "tool not found");
}

/// Whistle's transport is the launcher's, not the shepherd's, so it answers
/// `initialize` against a home with no daemon and no socket, and reports the
/// missing shepherd per call.
#[test]
fn whistle_starts_with_no_shepherd_and_reports_it_per_call() {
    let home = TempDir::new().unwrap();
    let stdin = mcp_session(&[call_tool_request(2, "list_flock", None)]);
    let output = shep(home.path())
        .arg("whistle")
        .write_stdin(stdin)
        .output()
        .unwrap();
    assert_success(&output);

    let lines = assert_every_stdout_line_is_jsonrpc(&output.stdout);

    let init_reply = find_reply(&lines, 1);
    assert_eq!(init_reply["result"]["serverInfo"]["name"], "shep");
    assert!(init_reply["result"]["capabilities"]["tools"].is_object());

    let call_reply = find_reply(&lines, 2);
    assert_eq!(call_reply["result"]["isError"], true);
    let message = call_reply["result"]["structuredContent"]["message"]
        .as_str()
        .expect("a no-shepherd refusal carries a message");
    assert!(
        message.contains("no shepherd is running"),
        "message: {message}"
    );
}

// --- Dogs / Available index -----------------------------------------------

/// Rex's description carries a raw `\u{1b}[2J` screen-clear escape. The
/// assertion is on raw stdout bytes, so a regression cannot hide behind
/// `String::from_utf8_lossy`'s replacement character.
#[test]
fn available_dogs_lists_the_index_and_never_leaks_a_raw_escape() {
    let home = TempDir::new().unwrap();
    let url = serve_dog_index(&two_entry_index_json());

    let output = shep(home.path())
        .env("SHEP_DOG_INDEX", &url)
        .arg("dogs")
        .arg("--available")
        .output()
        .unwrap();
    assert_success(&output);

    assert!(
        !output.stdout.contains(&0x1b),
        "a raw escape reached stdout: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "NAME",
        "PACKAGE",
        "CATEGORY",
        "DESCRIPTION",
        "Spot",
        "shep-log-rotate",
        "logs",
        "Rex",
        "shep-watchdog",
        "health",
    ] {
        assert!(
            stdout.contains(expected),
            "table is missing {expected:?}: {stdout}"
        );
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("1 entry contained control characters"),
        "stderr must note the sanitised entry: {stderr}"
    );
}

/// A dog cannot learn the name it was adopted under, so a wrong name here
/// ships a copy-pasteable command that discards its whole config section.
#[test]
fn available_dogs_detail_view_uses_adopt_as_never_name() {
    let home = TempDir::new().unwrap();
    let url = serve_dog_index(&two_entry_index_json());

    let output = shep(home.path())
        .env("SHEP_DOG_INDEX", &url)
        .arg("dogs")
        .arg("--available")
        .arg("spot")
        .output()
        .unwrap();
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("Spot . shep-log-rotate . logs"),
        "detail header line: {stdout}"
    );
    assert!(
        stdout.contains("$ cargo install --git https://github.com/shep-pm/shep-log-rotate"),
        "install command: {stdout}"
    );
    assert!(
        stdout.contains("$ shep adopt ~/.cargo/bin/shep-log-rotate --name log-rotate"),
        "adopt command must use adopt_as (log-rotate), not name (Spot): {stdout}"
    );
    assert!(
        !stdout.contains("--name Spot"),
        "adopt command must never use the display name: {stdout}"
    );
}

#[test]
fn available_dogs_zero_matches_exits_zero_and_says_so() {
    let home = TempDir::new().unwrap();
    let url = serve_dog_index(&two_entry_index_json());

    let output = shep(home.path())
        .env("SHEP_DOG_INDEX", &url)
        .arg("dogs")
        .arg("--available")
        .arg("wombat")
        .output()
        .unwrap();
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("no dog matches \"wombat\""),
        "stdout: {stdout}"
    );
}

/// Neither a socket nor a pidfile may exist afterwards, so an autostart is
/// caught even when the command still answers successfully.
#[test]
fn available_dogs_needs_no_shepherd() {
    let home = TempDir::new().unwrap();
    let url = serve_dog_index(&two_entry_index_json());

    let output = shep(home.path())
        .env("SHEP_DOG_INDEX", &url)
        .arg("dogs")
        .arg("--available")
        .output()
        .unwrap();
    assert_success(&output);
    assert!(
        !home.path().join("run").join("shep.sock").exists(),
        "--available must never bring up a shepherd"
    );
    assert!(
        !home.path().join("pids").join("shepd.pid").exists(),
        "--available must never bring up a shepherd"
    );
}

/// `IndexError` carries the URL on no variant but `InsecureUrl`, so
/// `available_dogs` is what names it.
#[test]
fn available_dogs_reports_a_server_error_naming_the_url() {
    let home = TempDir::new().unwrap();
    let url = serve_raw_response(
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n".to_string(),
    );

    let output = shep(home.path())
        .env("SHEP_DOG_INDEX", &url)
        .arg("dogs")
        .arg("--available")
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "a 500 must not exit success: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!("reading the dog index from {url}")),
        "stderr must name the failing url: {stderr}"
    );
    assert!(stderr.contains("500"), "stderr: {stderr}");
}

/// The one url `available_dogs` does not name. `SHEP_DOG_INDEX` is an
/// operator's own string, so a password can reach it, and this message is
/// built outside `fetch` and outside `IndexError` where neither refusal
/// covers it.
#[test]
fn available_dogs_names_no_url_that_carries_credentials() {
    // A sentinel per component, none of them a substring of anything the
    // message says on its own. A password redacted while the username or
    // the host it was paired with still prints is a narrower leak, not a
    // closed one.
    for url in [
        "ftp://sentineluser:hunter2@sentinelhost.invalid/dogs.json",
        // Scheme-relative, so there is no `://` to split the authority on.
        "//sentineluser:hunter2@sentinelhost.invalid/dogs.json",
    ] {
        let home = TempDir::new().unwrap();

        let output = shep(home.path())
            .env("SHEP_DOG_INDEX", url)
            .arg("dogs")
            .arg("--available")
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "{url}: an unfetchable url must not exit success: {output:?}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        for secret in ["hunter2", "sentineluser", "sentinelhost.invalid"] {
            assert!(
                !stderr.contains(secret),
                "{url}: stderr printed {secret}: {stderr}"
            );
        }
        assert!(
            stderr.contains("a url carrying credentials"),
            "{url}: stderr must say why it withheld the url: {stderr}"
        );
    }
}

#[test]
fn available_dogs_reports_a_truncated_body_naming_the_url() {
    let home = TempDir::new().unwrap();
    // Declares 100 bytes of body, sends 2, then closes: `fetch::get`'s
    // `Truncated` refusal.
    let url = serve_raw_response("HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n[]".to_string());

    let output = shep(home.path())
        .env("SHEP_DOG_INDEX", &url)
        .arg("dogs")
        .arg("--available")
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "a truncated body must not exit success: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!("reading the dog index from {url}")),
        "stderr must name the failing url: {stderr}"
    );
    assert!(stderr.contains("truncated"), "stderr: {stderr}");
}

// --- `shep serve` --------------------------------------------------------

#[cfg(unix)]
/// The assertion is an HTTP GET against the port, not a `shep flock` row: a
/// row says the process is up, and up is not serving.
#[test]
fn serve_registers_a_sheep_that_answers_on_its_port() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("site");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("index.html"), "hello from shep serve").unwrap();
    let mut guard = DaemonGuard::default();
    let port = free_port();

    let output = shep(dir.path())
        .arg("--format")
        .arg("json")
        .arg("serve")
        .arg(&root)
        .arg("--port")
        .arg(port.to_string())
        .output()
        .unwrap();
    guard.adopt_home(dir.path());
    assert_success(&output);

    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let (status, body) = poll_http_get(addr, "/", &[]);
    assert_eq!(status, 200, "body={body}");
    assert!(body.contains("hello from shep serve"), "{body}");

    graceful_kill(dir.path());
}

#[cfg(unix)]
#[test]
fn serve_refuses_a_docroot_that_is_not_a_directory() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope");
    let mut guard = DaemonGuard::default();

    let output = shep(dir.path())
        .arg("--format")
        .arg("json")
        .arg("serve")
        .arg(&missing)
        .output()
        .unwrap();
    guard.adopt_home(dir.path());

    assert_json_error(&output, 2, "usage");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(&missing.display().to_string()), "{stderr}");

    // No daemon was ever spawned to register anything against, so the
    // refusal happened before any `Request::Start`.
    assert!(
        daemon_pid(dir.path()).is_none(),
        "a refused root must not even bring a shepherd up"
    );
}

#[cfg(unix)]
/// A worker that only handles SIGINT rides the kill ladder to SIGKILL on
/// every `shep stop`. [`SERVE_STOP_DEADLINE`] carries the bound's basis.
#[test]
fn a_served_sheep_stops_on_sigterm_rather_than_riding_the_ladder_to_sigkill() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("site");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("index.html"), "ok").unwrap();
    let mut guard = DaemonGuard::default();
    let port = free_port();
    let name = "sigterm-check";

    let output = shep(dir.path())
        .arg("serve")
        .arg(&root)
        .arg("--port")
        .arg(port.to_string())
        .arg("--name")
        .arg(name)
        .output()
        .unwrap();
    guard.adopt_home(dir.path());
    assert_success(&output);

    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let (status, body) = poll_http_get(addr, "/", &[]);
    assert_eq!(status, 200, "body={body}");

    let started = Instant::now();
    let stop_output = shep(dir.path()).arg("stop").arg(name).output().unwrap();
    let elapsed = started.elapsed();
    assert_success(&stop_output);
    assert!(
        elapsed < SERVE_STOP_DEADLINE,
        "shep stop took {elapsed:?}, at or past SERVE_STOP_DEADLINE ({SERVE_STOP_DEADLINE:?}); \
         a worker riding the ladder to SIGKILL takes at least the 1600ms kill_timeout default"
    );

    graceful_kill(dir.path());
}

#[cfg(unix)]
/// Layout shared by the two `--follow-symlinks` cases below: a dated release
/// directory holding `index.html`, and a `current` symlink pointing at it.
fn write_deploy_layout(root: &Path) {
    let release = root.join("releases").join("2026-08-15");
    std::fs::create_dir_all(&release).unwrap();
    std::fs::write(release.join("index.html"), "the deploy layout").unwrap();
    std::os::unix::fs::symlink(&release, root.join("current")).unwrap();
}

#[cfg(unix)]
/// Registered without `--follow-symlinks`. A registered sheep is a real child
/// with its own captured stderr, which is what `shep bleats` reads.
#[test]
fn a_refused_symlink_writes_the_path_and_the_flag_to_the_sheeps_bleats() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("site");
    std::fs::create_dir(&root).unwrap();
    write_deploy_layout(&root);
    let canonical_root = root.canonicalize().unwrap();
    let mut guard = DaemonGuard::default();
    let port = free_port();
    let name = "symlink-refused";

    let output = shep(dir.path())
        .arg("serve")
        .arg(&root)
        .arg("--port")
        .arg(port.to_string())
        .arg("--name")
        .arg(name)
        .output()
        .unwrap();
    guard.adopt_home(dir.path());
    assert_success(&output);

    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let (status, body) = poll_http_get(addr, "/current/index.html", &[]);
    assert_eq!(status, 404, "body={body}");

    let bleats_output = bleats_no_follow_until_written(dir.path(), &[name, "--err"]);
    let bleats = String::from_utf8_lossy(&bleats_output.stdout);
    assert!(
        bleats.contains(&canonical_root.join("current").display().to_string()),
        "{bleats}"
    );
    assert!(bleats.contains("--follow-symlinks"), "{bleats}");

    graceful_kill(dir.path());
}

#[cfg(unix)]
/// One scenario: the flag that makes the deploy layout work is the flag
/// `follow_symlinks_notice` announces.
#[test]
fn a_served_sheep_with_follow_symlinks_serves_the_deploy_layout_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("site");
    std::fs::create_dir(&root).unwrap();
    write_deploy_layout(&root);
    let mut guard = DaemonGuard::default();
    let port = free_port();
    let name = "symlink-followed";

    let output = shep(dir.path())
        .arg("serve")
        .arg(&root)
        .arg("--port")
        .arg(port.to_string())
        .arg("--name")
        .arg(name)
        .arg("--follow-symlinks")
        .output()
        .unwrap();
    guard.adopt_home(dir.path());
    assert_success(&output);

    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let (status, body) = poll_http_get(addr, "/current/index.html", &[]);
    assert_eq!(status, 200, "body={body}");
    assert!(body.contains("the deploy layout"), "{body}");

    let bleats_output = bleats_no_follow_until_written(dir.path(), &[name, "--err"]);
    let bleats = String::from_utf8_lossy(&bleats_output.stdout);
    assert!(bleats.contains("--follow-symlinks"), "{bleats}");
    assert!(
        bleats.contains("race") || bleats.contains("TOCTOU"),
        "{bleats}"
    );

    graceful_kill(dir.path());
}

/// Two runs in one test so the `Command::cargo_bin` lookup is paid once: an
/// app that exits 0 with `autorestart = false` (exit 0), and one that exits 1
/// with `max_restarts = 1` (exit 11), which errors on the first unstable exit
/// so neither run waits through a restart delay. Each takes at least 6 seconds
/// (`commands::empty::STRIKES` × `INTERVAL`).
#[test]
fn runtime_exits_when_the_flock_empties_with_a_code_that_says_why() {
    // Clean emptying: one app exits 0 and is told not to restart.
    let clean_dir = tempfile::tempdir().unwrap();
    let clean_script = write_script(&clean_dir, "clean.sh", "#!/bin/sh\nexit 0\n");
    let clean_flockfile = write_flockfile(
        &clean_dir,
        &format!(
            "[[app]]\nname = \"batch\"\nscript = '{}'\nautorestart = false\n",
            clean_script.display(),
        ),
    );
    let clean = shep(clean_dir.path())
        .arg("runtime")
        .arg(&clean_flockfile)
        .output()
        .unwrap();
    assert_eq!(
        clean.status.code(),
        Some(0),
        "a clean emptying is not a failure; stderr={}",
        String::from_utf8_lossy(&clean.stderr)
    );

    // Fail-fast emptying: one app exits 1 with no restart budget at all.
    let failed_dir = tempfile::tempdir().unwrap();
    let failed_script = write_script(&failed_dir, "fail.sh", "#!/bin/sh\nexit 1\n");
    let failed_flockfile = write_flockfile(
        &failed_dir,
        &format!(
            "[[app]]\nname = \"batch\"\nscript = '{}'\nmax_restarts = 1\n",
            failed_script.display(),
        ),
    );
    let failed = shep(failed_dir.path())
        .arg("runtime")
        .arg(&failed_flockfile)
        .output()
        .unwrap();
    assert_eq!(
        failed.status.code(),
        Some(11),
        "an errored sheep must fail the container; stderr={}",
        String::from_utf8_lossy(&failed.stderr)
    );
}

#[cfg(unix)]
/// Fails if `shep runtime` serves a dog the compiled default instead of the
/// bind an operator wrote. `runtime` reaches `boot_supervisor` directly, never
/// `run_daemon`, so the dog-config migration has to run on both paths; a
/// container that only runs `shep runtime` otherwise brings its dogs up on
/// compiled defaults with no warning and no file written.
///
/// `#[cfg(unix)]`: [`wait_for_dog_pid`] uses `nix::unistd::Pid`.
#[test]
fn runtime_migrates_dog_config_and_serves_the_bind_an_operator_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let port = free_port();
    write_shep_toml(
        &dir,
        &format!(
            "[daemon]\nenabled_dogs = [\"metrics\"]\n\n[dog.metrics]\nbind = \"127.0.0.1:{port}\"\n"
        ),
    );
    let script = write_test_script(&dir);
    let flockfile = write_flockfile(
        &dir,
        &format!("[[app]]\nname = \"web\"\nscript = '{}'\n", script.display()),
    );
    let mut guard = DaemonGuard::default();
    // Before the spawn, not after: `runtime` boots its shepherd in its own
    // process, so there is a supervisor to reap from the moment it starts.
    guard.adopt_home(home);

    let mut child = std::process::Command::cargo_bin("shep")
        .expect("locate the built shep binary")
        .arg("--home")
        .arg(home)
        .arg("runtime")
        .arg(&flockfile)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn shep runtime");
    // `runtime` streams the flock's bleats to its own stdout for as long as
    // it runs; nothing draining those pipes wedges the child once they fill.
    discard_in_background(child.stdout.take().expect("piped stdout"));
    discard_in_background(child.stderr.take().expect("piped stderr"));

    // `wait_for_dog_pid` asserts success on its first `shep flock`, so it
    // cannot be the first thing aimed at a shepherd still booting.
    let start = Instant::now();
    while !shep(home).arg("flock").output().unwrap().status.success() {
        assert!(
            start.elapsed() < FLOCK_DEADLINE,
            "`shep runtime` never brought a shepherd up at {}",
            home.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let dog_pid = wait_for_dog_pid(home, "metrics");
    guard.adopt_dog_pid(dog_pid);

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let body = poll_metrics(addr);
    assert!(
        body.contains("HTTP/1.1 200"),
        "the metrics dog must answer at the bind shep.toml asked for, not at \
         the compiled default: {body}"
    );
    assert!(
        home.join("dogs.toml").is_file(),
        "`shep runtime` must migrate `[dog.metrics]` out of shep.toml"
    );

    graceful_kill(home);
    let _ = child.wait();
}

// --- `shep dev` -------------------------------------------------------

/// A `shep dev` invocation with `$SHEP_DEV_HOME` set to `dev_home`, timeout
/// already attached. Never `--home`: `dev` ignores it.
fn shep_dev(dev_home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("shep").unwrap();
    cmd.env("SHEP_DEV_HOME", dev_home)
        .arg("dev")
        .timeout(CMD_TIMEOUT);
    cmd
}

/// Copies `source` to nowhere, on a background thread. An undrained pipe
/// fills and blocks the child.
fn discard_in_background<R: Read + Send + 'static>(mut source: R) {
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut source, &mut std::io::sink());
    });
}

/// Spawns `shep dev <flockfile>` with `$SHEP_DEV_HOME` set to `dev_home`,
/// stdout and stderr piped and drained in the background. Leaves the process
/// alive, so a caller can signal it.
fn spawn_shep_dev(dev_home: &Path, flockfile: &Path) -> Child {
    let mut child = std::process::Command::cargo_bin("shep")
        .expect("locate the built shep binary")
        .env("SHEP_DEV_HOME", dev_home)
        .arg("dev")
        .arg(flockfile)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn shep dev");
    discard_in_background(child.stdout.take().unwrap());
    discard_in_background(child.stderr.take().unwrap());
    child
}

/// Polls `shep --home <dev_home> --format json flock` until the one app's row
/// reports `online`. Tolerates the early window before `shep dev` has bound
/// its socket, unlike [`poll_flock_data`].
fn wait_for_dev_online(dev_home: &Path, deadline: Duration) -> serde_json::Value {
    let start = Instant::now();
    loop {
        let output = shep(dev_home)
            .arg("--format")
            .arg("json")
            .arg("flock")
            .output()
            .unwrap();
        if output.status.success()
            && let Ok(envelope) = serde_json::from_slice::<serde_json::Value>(&output.stdout)
            && envelope["data"][0]["status"] == "online"
        {
            return envelope["data"][0].clone();
        }
        if start.elapsed() >= deadline {
            panic!(
                "shep dev's flock never reached online within {deadline:?}; last stdout={}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Polls `child.try_wait()` until it exits, or `timeout` elapses with a named
/// panic. `CMD_TIMEOUT`'s kill lives inside `.output()`, which
/// [`spawn_shep_dev`] never calls.
fn wait_bounded(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("poll shep dev") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("shep dev did not exit within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The auto-exit fires after `commands::empty::STRIKES` × `INTERVAL` (3 × 2s).
/// The setting under test is `tidy_up: true`, which reddens the no-shepherd
/// assertion when flipped, not the socket one. `$SHEP_DEV_HOME` points at its
/// own tempdir, never the real `~/.shep-dev`.
#[test]
fn dev_tidies_up_after_itself() {
    let dir = tempfile::tempdir().unwrap();
    let dev_home = tempfile::tempdir().unwrap();
    let script = write_script(&dir, "batch.sh", "#!/bin/sh\nexit 0\n");
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"batch\"\nscript = '{}'\nautorestart = false\n",
            script.display(),
        ),
    );

    let output = shep_dev(dev_home.path()).arg(&flockfile).output().unwrap();
    assert_success(&output);

    let socket = dev_home.path().join("run").join("shep.sock");
    assert!(!socket.exists(), "dev must not leave a live socket behind");

    let flock_output = shep(dev_home.path()).arg("flock").output().unwrap();
    assert!(
        !flock_output.status.success(),
        "no shepherd should remain at the dev home to answer `flock`: {flock_output:?}"
    );
}

#[cfg(unix)]
/// A signal reaches `commands::foreground::run`'s `RunningDaemon::run`
/// teardown directly, never the `Stop`/`Delete` pair `tidy_up` sends over the
/// wire, so `BootOptions::delete_flock_on_shutdown` is what keeps `flock.json`
/// from still listing the sheep as running.
#[test]
fn dev_tidies_up_when_it_is_signalled_rather_than_when_the_flock_empties() {
    let dir = tempfile::tempdir().unwrap();
    let dev_home = tempfile::tempdir().unwrap();
    let script = write_script(&dir, "held.sh", "#!/bin/sh\nsleep 60\n");
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"held\"\nscript = '{}'\n",
            script.display()
        ),
    );

    let mut child = spawn_shep_dev(dev_home.path(), &flockfile);
    let dev_pid = child.id() as i32;

    let online = wait_for_dev_online(dev_home.path(), FLOCK_DEADLINE);
    let sheep_pid = online["pid"]
        .as_i64()
        .unwrap_or_else(|| panic!("a real pid: {online}")) as i32;

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(dev_pid),
        nix::sys::signal::Signal::SIGTERM,
    )
    .expect("send SIGTERM to shep dev");

    let status = wait_bounded(&mut child, FLOCK_DEADLINE);
    assert!(
        status.success(),
        "a signalled dev session must still tidy up and exit cleanly: {status:?}"
    );

    let socket = dev_home.path().join("run").join("shep.sock");
    assert!(!socket.exists(), "dev must not leave a live socket behind");

    let flock_output = shep(dev_home.path()).arg("flock").output().unwrap();
    assert!(
        !flock_output.status.success(),
        "no shepherd should remain at the dev home to answer `flock`: {flock_output:?}"
    );

    assert!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(sheep_pid), None).is_err(),
        "the held sheep (pid {sheep_pid}) must not outlive the dev session"
    );

    let roll_text = std::fs::read_to_string(dev_home.path().join("flock.json"))
        .expect("teardown must still write a final flock.json, even an empty one");
    let roll: serde_json::Value =
        serde_json::from_str(&roll_text).expect("flock.json must still be valid JSON");
    assert_eq!(
        roll["apps"].as_array().map(Vec::len),
        Some(0),
        "a signalled dev session must not leave `held` in the roll for `shep muster` to \
         resurrect: {roll}"
    );
}

/// The assertion is the usage line, not the verb's name: the root `shep
/// --help` lists `dev` and `runtime` among its subcommands, so
/// `text.contains("dev")` passes even with `alias_argv` deleted.
#[test]
fn the_alias_binaries_exist_and_reach_their_own_verbs() {
    for (bin, verb) in [("shep-dev", "dev"), ("shep-runtime", "runtime")] {
        let output = Command::cargo_bin(bin)
            .unwrap_or_else(|err| panic!("{bin} must be a [[bin]] target: {err}"))
            .arg("--help")
            .timeout(CMD_TIMEOUT)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            text.contains(&format!("Usage: shep {verb}")),
            "{bin} --help must be {verb}'s own help, not the root's:\n{text}"
        );
        assert!(
            !text.contains("lookout"),
            "{bin} printed the root verb list, so the alias supplied no verb:\n{text}"
        );
    }
}

// --- Piped stdout stays bare ----------------------------------------------

/// Asserts no ANSI escape byte and none of the box-drawing glyphs
/// `render_boxed` draws.
fn assert_no_box_or_escape_reached_the_pipe(stdout: &str, verb: &str) {
    assert!(
        !stdout.contains('\u{1b}'),
        "shep {verb} piped: an escape byte reached a pipe: {stdout:?}"
    );
    for glyph in ['┌', '┬', '┐', '├', '┼', '┤', '└', '┴', '┘', '│', '─'] {
        assert!(
            !stdout.contains(glyph),
            "shep {verb} piped: a box-drawing glyph ({glyph:?}) reached a pipe:\n{stdout}"
        );
    }
}

#[cfg(unix)]
/// The only place in the suite a table verb runs with no `--format json` and
/// no `--style`. `.output()` captures stdout through an OS pipe, never a pty,
/// so `std::io::stdout().is_terminal()` is `false`, which is
/// `must_render_bare`'s trigger. Two verbs, since `emit_flock` and
/// `emit_described` wrap `table_of` separately and a regression scoped to one
/// would pass a case trying the other.
#[test]
fn piped_table_output_at_the_default_style_carries_no_box_or_escape() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();

    let started = shep(dir.path())
        .arg("--format")
        .arg("json")
        .arg("start")
        .arg(&script)
        .output()
        .unwrap();
    guard.adopt_home(dir.path());
    assert_success(&started);
    let envelope: serde_json::Value = serde_json::from_slice(&started.stdout).unwrap();
    assert_eq!(envelope["data"][0]["status"], "online", "{envelope}");

    let flock = shep(dir.path()).arg("flock").output().unwrap();
    assert_success(&flock);
    let flock_stdout = String::from_utf8_lossy(&flock.stdout).into_owned();
    assert_no_box_or_escape_reached_the_pipe(&flock_stdout, "flock");
    assert!(
        flock_stdout.contains("online"),
        "precondition: the piped table must still say something: {flock_stdout}"
    );

    let describe = shep(dir.path())
        .arg("describe")
        .arg("all")
        .output()
        .unwrap();
    assert_success(&describe);
    let describe_stdout = String::from_utf8_lossy(&describe.stdout).into_owned();
    assert_no_box_or_escape_reached_the_pipe(&describe_stdout, "describe");
    assert!(
        describe_stdout.contains("online"),
        "precondition: the piped table must still say something: {describe_stdout}"
    );

    graceful_kill(dir.path());
}

// --- Issue 1/2/3: adopt ergonomics and `shep <dogname>` dispatch ---------

/// Spells a path the way shep spells it: canonicalized, with Windows' `\?\`
/// prefix stripped back off so `shep.toml` stays hand-editable, and 8.3 short
/// names expanded (`%TEMP%` on a Windows runner is `C:\Users\RUNNER~1\...`,
/// which canonicalizes to `runneradmin`).
fn as_shep_spells_it(path: &Path) -> String {
    let canonical = std::fs::canonicalize(path).expect("canonicalize the recorded binary");
    shep_core::paths::strip_verbatim_prefix(&canonical)
        .display()
        .to_string()
}

#[test]
fn shep_adopt_finds_a_binary_on_path_by_bare_name() {
    let home = TempDir::new().unwrap();
    let bin_dir = TempDir::new().unwrap();
    let binary = write_script(&bin_dir, "shep-log-rotate", "#!/bin/sh\nexit 0\n");

    let output = Command::cargo_bin("shep")
        .unwrap()
        .env("PATH", bin_dir.path())
        .arg("--home")
        .arg(home.path())
        .arg("adopt")
        .arg("shep-log-rotate")
        .arg("--name")
        .arg("lr")
        .timeout(CMD_TIMEOUT)
        .output()
        .unwrap();

    assert_success(&output);
    let written = std::fs::read_to_string(home.path().join("shep.toml")).unwrap();
    assert!(
        written.contains(&as_shep_spells_it(&binary)),
        "the $PATH hit must be the recorded binary: {written}"
    );
}

#[cfg(unix)]
/// A literal `~/` path, expanded by `shep adopt` as it is in a Flockfile.
#[test]
fn shep_adopt_expands_a_leading_tilde_path() {
    let shep_home = TempDir::new().unwrap();
    let fake_user_home = TempDir::new().unwrap();
    let bin_dir = fake_user_home.path().join(".cargo").join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let binary = bin_dir.join("shep-log-rotate");
    std::fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
    let mut mode = std::fs::metadata(&binary).unwrap().permissions();
    mode.set_mode(0o755);
    std::fs::set_permissions(&binary, mode).unwrap();

    let output = Command::cargo_bin("shep")
        .unwrap()
        .env("HOME", fake_user_home.path())
        .arg("--home")
        .arg(shep_home.path())
        .arg("adopt")
        .arg("~/.cargo/bin/shep-log-rotate")
        .arg("--name")
        .arg("lr")
        .timeout(CMD_TIMEOUT)
        .output()
        .unwrap();

    assert_success(&output);
    let written = std::fs::read_to_string(shep_home.path().join("shep.toml")).unwrap();
    assert!(
        written.contains(&as_shep_spells_it(&binary)),
        "the ~/-expanded binary must be the one recorded: {written}"
    );
}
/// Writes a script that records its own argv and `$SHEP_HOME` into `marker`
/// (inside `dir`), prints a distinctive stdout line, and exits `code`.
fn write_marker_script(dir: &TempDir, marker: &Path, code: u8) -> PathBuf {
    // `$*`/`$SHEP_HOME` in a shell script, `%*`/`%SHEP_HOME%` in a `.cmd`.
    // No space before `>` in the batch arm: `echo foo > x` writes a trailing
    // space in `cmd.exe`, and the assertion is on an exact line.
    #[cfg(unix)]
    let body = format!(
        "#!/bin/sh\necho \"argv:$*\" > \"{marker}\"\necho \"home:$SHEP_HOME\" >> \"{marker}\"\necho from-the-dog\nexit {code}\n",
        marker = marker.display(),
    );
    #[cfg(windows)]
    let body = format!(
        "@echo off\r\necho argv:%*>\"{marker}\"\r\necho home:%SHEP_HOME%>>\"{marker}\"\r\necho from-the-dog\r\nexit /b {code}\r\n",
        marker = marker.display(),
    );
    write_script(dir, "dog.sh", &body)
}

/// `shep <dogname> [args...]` runs an adopted dog with the operator's argv
/// passed through untouched and `$SHEP_HOME` set. The dispatch call carries no
/// `--home`, exercising `home_before`'s fallback to the real environment.
#[test]
fn an_adopted_dog_runs_directly_with_its_own_argv_and_shep_home() {
    let home = TempDir::new().unwrap();
    let marker = home.path().join("marker.txt");
    let script = write_marker_script(&home, &marker, 7);

    let adopted = shep(home.path())
        .arg("adopt")
        .arg(&script)
        .arg("--name")
        .arg("deploy")
        .output()
        .unwrap();
    assert_success(&adopted);

    let ran = Command::cargo_bin("shep")
        .unwrap()
        .env("SHEP_HOME", home.path())
        .arg("deploy")
        .arg("koji")
        .arg("--flag")
        .timeout(CMD_TIMEOUT)
        .output()
        .unwrap();

    assert_eq!(
        ran.status.code(),
        Some(7),
        "the dog's own exit code must pass through: {ran:?}"
    );
    assert!(
        String::from_utf8_lossy(&ran.stdout).contains("from-the-dog"),
        "stdio must be inherited, not captured away: {ran:?}"
    );
    let recorded = std::fs::read_to_string(&marker).unwrap();
    assert!(
        recorded.contains("argv:koji --flag"),
        "argv must reach the dog exactly as typed: {recorded}"
    );
    assert!(
        recorded.contains(&format!("home:{}", home.path().display())),
        "SHEP_HOME must reach the dog's own environment: {recorded}"
    );
}

/// `dispatch_adopted_dog` runs only once clap has failed to match a token
/// against a real subcommand, so an adopted dog named `stop` never shadows the
/// verb. Exit 5 (`DaemonUnreachable`, since `stop` does not autostart) and the
/// marker file never appearing are what say the built-in was dispatched.
#[test]
fn a_built_in_verb_always_wins_over_a_same_named_adopted_dog() {
    let home = TempDir::new().unwrap();
    let marker = home.path().join("marker.txt");
    let script = write_marker_script(&home, &marker, 0);
    std::fs::write(
        home.path().join("shep.toml"),
        format!(
            "[daemon]\nadopted_dogs = {{ stop = \"{}\" }}\nenabled_dogs = [\"stop\"]\n",
            script.display()
        ),
    )
    .unwrap();

    let output = shep(home.path()).arg("stop").arg("all").output().unwrap();

    assert_eq!(
        output.status.code(),
        Some(5),
        "must be the built-in `stop`'s own DaemonUnreachable, not the dog's exit 0: {output:?}"
    );
    assert!(
        !marker.exists(),
        "the adopted dog's script must never have run"
    );
}

/// `dispatch_adopted_dog` finding nothing falls through to clap's own
/// unknown-verb rendering, suggestions included.
#[test]
fn an_unknown_verb_with_no_matching_dog_keeps_claps_own_suggestion() {
    let home = TempDir::new().unwrap();

    let output = shep(home.path()).arg("flcok").output().unwrap();

    assert_eq!(output.status.code(), Some(2), "clap's own usage exit code");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unrecognized subcommand"),
        "clap's own wording must survive untouched: {stderr}"
    );
    assert!(
        stderr.contains("flock"),
        "clap's own did-you-mean must still suggest the real verb: {stderr}"
    );
}

/// A Flockfile edit reaches a registered sheep only where the first load
/// established nothing, and what it reaches is reported by name, never by
/// value. A key the first load established belongs to whoever set it. A key
/// nobody has established is appended, applied where it can be and parked
/// where it cannot, and the line naming a parked field names `shep reload`.
#[test]
fn a_flockfile_edit_reaches_a_sheep_only_where_the_first_load_established_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let elsewhere = tempfile::tempdir().unwrap();
    let script = write_test_script(&dir);
    // `env` carries a value that must never be printed, and the third file
    // changes two fields at once, so a report that stopped at the first
    // difference fails here too.
    let body = |cwd: &Path, env: &str, extra: &str| {
        format!(
            "[[app]]\nname = \"edited\"\nscript = '{}'\ncwd = '{}'\n{extra}env = {{ {env} }}\n",
            script.display(),
            cwd.display(),
        )
    };
    let flockfile = write_flockfile(&dir, &body(home, "API_TOKEN = \"hunter2-before\"", ""));
    let mut guard = DaemonGuard::default();

    let boot = shep(home).arg("start").arg(&flockfile).output().unwrap();
    guard.adopt_home(home);
    assert_success(&boot);
    poll_flock(home, |info| info["status"] == "online");

    // The edit, over the same path the daemon was told about. The first load
    // established both keys, so this load may do nothing at all.
    write_flockfile(
        &dir,
        &body(elsewhere.path(), "API_TOKEN = \"hunter2-after\"", ""),
    );
    let again = shep(home).arg("start").arg(&flockfile).output().unwrap();
    // A load reports, it does not fail.
    assert_success(&again);
    let stderr = String::from_utf8_lossy(&again.stderr);
    assert!(
        !stderr.contains("cwd") && !stderr.contains("env"),
        "an established key is not the file's to change, so there is nothing \
         to report: {stderr}"
    );
    let info = poll_flock(home, |info| info["status"] == "online");
    assert!(
        info["pending"].is_null(),
        "an established key must not be parked either: {info}"
    );

    // A third load, adding a key nobody has established: one that reaches the
    // running child and one that cannot.
    write_flockfile(
        &dir,
        &body(
            elsewhere.path(),
            "API_TOKEN = \"hunter2-after\", MODE = \"blue\"",
            "max_memory = \"512M\"\n",
        ),
    );
    let third = shep(home).arg("start").arg(&flockfile).output().unwrap();
    assert_success(&third);

    let stderr = String::from_utf8_lossy(&third.stderr);
    assert!(
        stderr.contains("edited"),
        "the report must name the sheep: {stderr}"
    );
    assert!(
        stderr.contains("max_memory") && stderr.contains("env"),
        "the report must name every field that changed: {stderr}"
    );
    assert!(
        !stderr.contains("cwd"),
        "the established key is still nobody's to change: {stderr}"
    );
    assert!(
        !stderr.contains("hunter2") && !stderr.contains("blue"),
        "a field's VALUE must never reach an operator's terminal (IR-41): {stderr}"
    );
    // An env change is baked into a running child, so the report has to say
    // what brings it into effect, not only which field moved.
    assert!(
        stderr.contains("shep reload edited"),
        "a pending field travels with the verb that promotes it: {stderr}"
    );

    graceful_kill(home);
}

/// A load that refused one app exits non-zero, and still reports the app it
/// applied. The refusal is a real one: a plain load never reshapes a flock, so
/// a file that grows an `instances` line is refused that one field by name and
/// told which flag would take it.
#[test]
fn a_load_that_refused_one_app_exits_non_zero_and_still_reports_the_other() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script = write_test_script(&dir);
    // `steady` carries the field that lands, `stocky` the one that cannot.
    // `max_restarts` is read when a sheep exits, so it is in force the moment
    // it reaches the stored spec and reports as applied.
    let body = |steady: &str, stocky: &str| {
        format!(
            "[[app]]\nname = \"steady\"\nscript = '{}'\n{steady}\
             [[app]]\nname = \"stocky\"\nscript = '{}'\n{stocky}",
            script.display(),
            script.display(),
        )
    };
    let flockfile = write_flockfile(&dir, &body("", ""));
    let mut guard = DaemonGuard::default();

    let boot = shep(home).arg("start").arg(&flockfile).output().unwrap();
    guard.adopt_home(home);
    assert_success(&boot);
    poll_flock(home, |info| info["status"] == "online");

    // One field each: one the daemon applies, one it refuses.
    write_flockfile(&dir, &body("max_restarts = 9\n", "instances = 2\n"));
    let again = shep(home).arg("start").arg(&flockfile).output().unwrap();

    let stderr = String::from_utf8_lossy(&again.stderr);
    // Pinned at 4: `InvalidConfig` is the code the rest of this CLI uses for a
    // configuration the daemon would not accept.
    assert_eq!(
        again.status.code(),
        Some(4),
        "a refused load is a failed load, and an invalid-config one: {stderr}"
    );
    assert!(
        stderr.contains("stocky") && stderr.contains("reshapes a flock"),
        "the refusal names the app and what was refused: {stderr}"
    );
    assert!(
        stderr.contains("applied max_restarts"),
        "and the app that DID apply is still reported beside it: {stderr}"
    );

    graceful_kill(home);
}

/// One app whose script does not exist refuses the whole Flockfile, before
/// anything is registered, and names every app that failed. Without the check,
/// an app that fails to spawn partway through leaves the apps before it
/// registered and the apps after it unreached.
#[test]
fn one_absent_script_refuses_the_whole_flockfile_and_registers_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script = write_test_script(&dir);
    // `good` first, so a check that runs per app as it registers would have
    // registered it by the time it reached `unbuilt`.
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"good\"\nscript = '{}'\n\n\
             [[app]]\nname = \"unbuilt\"\nscript = '{}/never-built'\n",
            script.display(),
            dir.path().display(),
        ),
    );
    let mut guard = DaemonGuard::default();

    let output = shep(home).arg("start").arg(&flockfile).output().unwrap();
    guard.adopt_home(home);

    assert_eq!(
        output.status.code(),
        Some(7),
        "the spawn-failed exit code: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unbuilt"),
        "the refusal must name the app: {stderr}"
    );
    assert!(
        stderr.contains("never-built"),
        "the refusal must name the path it looked at: {stderr}"
    );

    let flock = shep(home)
        .arg("--format")
        .arg("json")
        .arg("flock")
        .output()
        .unwrap();
    assert_success(&flock);
    let envelope: serde_json::Value = serde_json::from_slice(&flock.stdout).unwrap();
    assert_eq!(
        envelope["data"].as_array().map(Vec::len),
        Some(0),
        "a Flockfile refused as a whole must leave NOTHING registered: {}",
        envelope
    );

    graceful_kill(home);
}

/// A spawn that fails for a reason no preflight could see still names the
/// sheep, the path it tried, and the `cwd` it tried it in. A bare
/// `SpawnFailed` names none of them.
///
/// The script exists and cannot be exec'd, so this reaches the real `spawn`
/// and its real `EACCES` rather than the batch existence check.
#[test]
fn a_spawn_that_no_check_could_have_caught_still_names_the_sheep_and_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let unrunnable = dir.path().join("unrunnable.sh");
    std::fs::write(&unrunnable, "#!/bin/sh\nsleep 60\n").unwrap();
    // Present, so the batch check passes it; no execute bit anywhere, which
    // even a root-owned run cannot exec.
    #[cfg(unix)]
    {
        std::fs::set_permissions(&unrunnable, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"locked-out\"\nscript = '{}'\n",
            unrunnable.display(),
        ),
    );
    let mut guard = DaemonGuard::default();

    let output = shep(home).arg("start").arg(&flockfile).output().unwrap();
    guard.adopt_home(home);

    assert_eq!(
        output.status.code(),
        Some(7),
        "the spawn-failed exit code: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("locked-out"),
        "the error must name the sheep: {stderr}"
    );
    assert!(
        stderr.contains("unrunnable.sh"),
        "the error must name the script it tried: {stderr}"
    );
    // Canonicalized: `start` fills an app's absent `cwd` from the
    // Flockfile's own directory through `canonicalize`, and on macOS a
    // tempdir's `/var/...` resolves to `/private/var/...`.
    let flockfile_dir = as_shep_spells_it(dir.path());
    assert!(
        stderr.contains(&format!("in {flockfile_dir}")),
        "the error must name the cwd it tried it in: {stderr}"
    );

    graceful_kill(home);
}

/// A bare command not on the shepherd's PATH is reported, fails to spawn, and
/// takes no other app in the Flockfile down with it. A `script` with a `/` in
/// it is a filesystem claim the daemon can settle, so that one is refused as a
/// batch instead. The `PATH` deciding a bare command is the daemon's: under a
/// `shep startup` unit, whatever launchd or systemd hands it, with
/// `assemble`'s fallback of `/usr/local/bin:/usr/bin:/bin`.
#[test]
fn a_bare_command_off_the_path_takes_only_its_own_app_down() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script = write_test_script(&dir);
    // `resolvable` first: it is the app that must survive, and a refusal of
    // the whole batch would leave it unregistered rather than online.
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"resolvable\"\nscript = '{}'\n\n\
             [[app]]\nname = \"no-interpreter\"\nscript = \"shep-no-such-interpreter-xyz\"\n",
            script.display(),
        ),
    );
    let mut guard = DaemonGuard::default();

    let output = shep(home).arg("start").arg(&flockfile).output().unwrap();
    guard.adopt_home(home);

    // Exit 7 all the same: one app really did fail.
    assert_eq!(
        output.status.code(),
        Some(7),
        "the one app that cannot run still fails the command: {output:?}"
    );
    // The useful sentence reaches the operator's terminal, not only the
    // shepherd's log; `SpawnFailed` carries free-form text.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("is not on the shepherd's PATH"),
        "the reply must explain WHY the program was not found, not only that \
         it was not: {stderr}"
    );
    assert!(
        stderr.contains("shep-no-such-interpreter-xyz") && stderr.contains("no-interpreter"),
        "naming the program and the sheep: {stderr}"
    );

    let data = poll_flock_data(home, FLOCK_DEADLINE, |data| {
        data.as_array().is_some_and(|rows| {
            rows.iter()
                .any(|row| row["name"] == "resolvable" && row["status"] == "online")
        })
    });
    // Found by hand rather than through `sheep_named`, which panics with its
    // own message: the regression makes the row absent, and a red run has to
    // say that.
    let survivor = data
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["name"] == "resolvable"));
    assert_eq!(
        survivor.map(|row| &row["status"]).map(ToString::to_string),
        Some("\"online\"".to_string()),
        "an app whose own script resolves must come up regardless of a \
         sibling's unresolvable interpreter, and must not be refused \
         registration over it: {data}"
    );

    let log = std::fs::read_to_string(home.join("logs").join("shepd.err.log")).unwrap();
    assert!(
        log.contains("shep-no-such-interpreter-xyz") && log.contains("PATH"),
        "the shepherd must still say which program it could not find: {log}"
    );
    assert!(
        log.contains("no-interpreter"),
        "and which sheep wanted it: {log}"
    );

    graceful_kill(home);
}

/// A real multi-instance flock through the real binary: distinct slots, a
/// grouped `shep flock` table, and a `merge_logs` app whose backlog prints each
/// line exactly once. A `shep bleats` that reads a file per matched row doubles
/// every line, which counting occurrences catches and a `contains` check does
/// not. A `sh`/`.cmd` script rather than node, so the case runs on Windows too.
#[test]
fn a_multi_instance_flock_gets_distinct_slots_a_grouped_table_and_undoubled_bleats() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let web = write_instance_logging_script(&dir, "web-instances", "web-slot");
    let merged = write_instance_logging_script(&dir, "merged-instances", "merged-slot");
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"web\"\nscript = '{}'\ninstances = 3\n\n\
             [[app]]\nname = \"merged\"\nscript = '{}'\ninstances = 2\nmerge_logs = true\n",
            web.display(),
            merged.display(),
        ),
    );
    let mut guard = DaemonGuard::default();

    let boot = shep(home).arg("start").arg(&flockfile).output().unwrap();
    guard.adopt_home(home);
    assert_success(&boot);

    // 1. Five processes, and every one of them reports the slot it occupies.
    let data = poll_flock_data(home, FLOCK_DEADLINE, |data| {
        data.as_array()
            .is_some_and(|rows| rows.iter().filter(|row| row["status"] == "online").count() == 5)
    });
    let slots_of = |name: &str| {
        let mut slots: Vec<u64> = data
            .as_array()
            .expect("flock data is an array")
            .iter()
            .filter(|row| row["name"] == name)
            .map(|row| {
                row["instance"]
                    .as_u64()
                    .unwrap_or_else(|| panic!("every instance reports a slot: {data}"))
            })
            .collect();
        slots.sort_unstable();
        slots
    };
    assert_eq!(slots_of("web"), vec![0, 1, 2], "distinct slots: {data}");
    assert_eq!(slots_of("merged"), vec![0, 1], "distinct slots: {data}");

    // 2. The table names each row by its own slot. The flat shape, not the
    // boxed one: `must_render_bare` drops any run whose stdout is not a
    // terminal to `StyleLevel::Bare`, and `--style plain` does not override it.
    let table = shep(home).arg("flock").output().unwrap();
    assert_success(&table);
    let rendered = String::from_utf8_lossy(&table.stdout);
    for slot in 0..3 {
        assert!(
            rendered.contains(&format!("web:{slot}")),
            "a row named for slot {slot}: {rendered}"
        );
    }
    assert!(
        rendered.contains("merged:0") && rendered.contains("merged:1"),
        "and the same for the merged app: {rendered}"
    );

    // 3. The regression guard. First, that `merge_logs` really did collapse
    // both instances onto one path: without that the count below is vacuous.
    let out_files: Vec<&str> = data
        .as_array()
        .expect("flock data is an array")
        .iter()
        .filter(|row| row["name"] == "merged")
        .map(|row| {
            row["out_file"]
                .as_str()
                .unwrap_or_else(|| panic!("a running sheep reports its out file: {data}"))
        })
        .collect();
    assert_eq!(out_files.len(), 2, "{data}");
    assert_eq!(
        out_files[0], out_files[1],
        "merge_logs points both instances at one file: {data}"
    );

    // Then that the shared file is read once, not once per matched row.
    let backlog =
        bleats_no_follow_until_contains(home, &["merged"], &["merged-slot-0", "merged-slot-1"]);
    assert_eq!(
        backlog.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&backlog.stderr)
    );
    let stdout = String::from_utf8_lossy(&backlog.stdout);
    for slot in 0..2 {
        let needle = format!("merged-slot-{slot}");
        assert_eq!(
            stdout.matches(&needle).count(),
            1,
            "a shared log file is read once, not once per instance: {stdout}"
        );
    }

    graceful_kill(home);
}

// --- Daemon handover -----------------------------------------------------

/// How long [`counting_lines`] waits for a counter sheep to reach a line
/// count, and [`wait_for_pid`] for a sheep to be online with a pid.
///
/// The counter emits five lines a second and the longest wait here is six
/// lines, so twenty seconds is a loaded runner's margin, not the sheep's pace.
#[cfg(unix)]
const HANDOVER_DEADLINE: Duration = Duration::from_secs(20);

/// Gap between [`counting_lines`]'s reads.
#[cfg(unix)]
const HANDOVER_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long [`a_sheep_owed_a_restart_still_gets_one_after_a_daemon_reload`]
/// waits for a re-armed backoff to fire.
///
/// The app's `restart_delay` is 8s and the successor re-arms with the same
/// figure, so five times that is a loaded runner's margin.
#[cfg(unix)]
const RESTARTED_DEADLINE: Duration = Duration::from_secs(40);

/// Writes a script that counts from 1 upwards on stdout, one number per line,
/// forever.
///
/// The sequence is what makes a log gap visible: a counting sheep proves
/// nothing between it and the file was lost, reordered or cut in half. A
/// restarted sheep starts again at 1.
#[cfg(unix)]
fn write_counting_script(dir: &TempDir) -> PathBuf {
    write_script(
        dir,
        "counter.sh",
        &format!(
            "{}{}i=1\nwhile :; do\n  echo \"$i\"\n  i=$((i+1))\n  sleep 0.2\ndone\n",
            script_header(),
            record_pid_line(dir),
        ),
    )
}

/// A log file's contents with the daemon's per-line timestamp taken back off,
/// through `shep_core::logstamp::strip`, the same call `shep bleats` makes.
fn unstamped_file(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap();
    let mut out = String::new();
    for line in text.lines() {
        out.push_str(shep_core::logstamp::strip(line));
        out.push('\n');
    }
    out
}

/// Reads `path` until it holds at least `want` lines, or
/// [`HANDOVER_DEADLINE`] expires, and returns what it held on the last read.
/// Returns rather than panicking on expiry, so the failure is the caller's own
/// assertion.
#[cfg(unix)]
fn counting_lines(path: &Path, want: usize) -> Vec<String> {
    let start = Instant::now();
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let lines: Vec<String> = text
            .lines()
            .map(shep_core::logstamp::strip)
            .map(str::to_owned)
            .collect();
        if lines.len() >= want || start.elapsed() >= HANDOVER_DEADLINE {
            return lines;
        }
        std::thread::sleep(HANDOVER_POLL_INTERVAL);
    }
}

/// Fails unless `lines` is `1, 2, 3, …`, one number per line, with nothing
/// missing, nothing repeated and nothing cut in half. The log pump reads
/// through a `BufReader`, so bytes consumed without yet forming a line die with
/// the process image and the successor's reader starts mid-line. The counter
/// emits five lines a second rather than as fast as it can: the handover's
/// flush empties the log file's write buffer, not the reader's.
#[cfg(unix)]
fn assert_unbroken_sequence(lines: &[String], what: &str) {
    for (index, line) in lines.iter().enumerate() {
        let want = index + 1;
        let got: usize = line.trim().parse().unwrap_or_else(|_| {
            panic!("{what}: line {want} is not a whole number, so it was torn: {line:?}")
        });
        assert_eq!(
            got,
            want,
            "{what}: expected {want} on line {want}, got {got}; the sequence so far is {:?}",
            &lines[..=index]
        );
    }
}

/// A sheep keeps its pid across `shep daemon reload`, and its log gains no
/// gap. Neither implies the other: a handover that respawned the sheep keeps
/// the log growing while moving the pid, and one that carried the pid while
/// dropping the pipe leaves the sheep blocked on `write()`.
///
/// Unix only, as the whole handover is: Windows has no `execve`.
#[cfg(unix)]
#[test]
fn a_sheep_keeps_its_pid_and_its_log_across_a_daemon_reload() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_counting_script(&dir);
    let mut guard = DaemonGuard::default();

    let started = shep(dir.path())
        .arg("--format")
        .arg("json")
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("counter")
        .output()
        .unwrap();
    guard.adopt_home(dir.path());
    assert_success(&started);

    let before = poll_flock(dir.path(), |info| {
        info["status"] == "online" && !info["pid"].is_null()
    });
    let pid_before = before["pid"]
        .as_u64()
        .unwrap_or_else(|| panic!("an online sheep reports a pid: {before}"));
    let out_file = PathBuf::from(
        before["out_file"]
            .as_str()
            .unwrap_or_else(|| panic!("an online sheep reports its out file: {before}")),
    );
    let seen_before = counting_lines(&out_file, 3);
    assert!(
        seen_before.len() >= 3,
        "the counter must be logging before the reload: {seen_before:?}"
    );

    let reloaded = shep(dir.path())
        .arg("daemon")
        .arg("reload")
        .output()
        .unwrap();
    assert_success(&reloaded);

    let after = poll_flock(dir.path(), |info| {
        info["status"] == "online" && !info["pid"].is_null()
    });
    assert_eq!(
        after["pid"].as_u64(),
        Some(pid_before),
        "a moved pid means the sheep was respawned, which is the stop arm: {after}"
    );

    let seen = counting_lines(&out_file, seen_before.len() + 3);
    assert!(
        seen.len() > seen_before.len(),
        "the sheep stopped logging across the handover: {seen:?}"
    );
    assert_unbroken_sequence(&seen, "the counter's log across a handover");

    graceful_kill(dir.path());
}

/// Fails if a bad `shep.toml` can orphan a running flock: the refusal must
/// happen before anything is signalled, on both the handover arm and the
/// stop-and-start arm. No `#[cfg(unix)]`, since the pre-flight in
/// `reload_with_wait` runs before the arm is chosen.
#[test]
fn a_bad_shep_toml_refuses_the_reload_and_leaves_the_flock_supervised() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();

    let started = shep(dir.path())
        .arg("--format")
        .arg("json")
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("sheep")
        .output()
        .unwrap();
    guard.adopt_home(dir.path());
    assert_success(&started);

    let before = poll_flock(dir.path(), |info| {
        info["status"] == "online" && !info["pid"].is_null()
    });
    let pid_before = before["pid"]
        .as_u64()
        .unwrap_or_else(|| panic!("an online sheep reports a pid: {before}"));

    write_shep_toml(&dir, "[daemon]\nmax_cron_sleep = \"soon\"\n");

    let reloaded = shep(dir.path())
        .arg("daemon")
        .arg("reload")
        .output()
        .unwrap();
    assert_eq!(
        reloaded.status.code(),
        Some(4),
        "InvalidConfig; stderr={}",
        String::from_utf8_lossy(&reloaded.stderr)
    );

    let after = shep(dir.path())
        .arg("--format")
        .arg("json")
        .arg("flock")
        .output()
        .unwrap();
    assert_success(&after);
    let envelope: serde_json::Value = serde_json::from_slice(&after.stdout).unwrap();
    let sheep = &envelope["data"][0];
    assert_eq!(sheep["status"], "online", "still supervised: {sheep}");
    assert_eq!(
        sheep["pid"].as_u64(),
        Some(pid_before),
        "the refusal must happen before anything is signalled: {sheep}"
    );

    graceful_kill(dir.path());
}

/// Fails if a dog section shep cannot move can orphan a running flock.
///
/// The dog-config migration runs at the top of every boot, so on the handover
/// arm it runs in a successor whose predecessor is already gone, and a refusal
/// there leaves the flock running with nothing supervising it. No
/// `#[cfg(unix)]`: the pre-flight runs before the arm is chosen.
#[test]
fn a_refused_dog_migration_refuses_the_reload_and_leaves_the_flock_supervised() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();

    let started = shep(dir.path())
        .arg("--format")
        .arg("json")
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("sheep")
        .output()
        .unwrap();
    guard.adopt_home(dir.path());
    assert_success(&started);

    let before = poll_flock(dir.path(), |info| {
        info["status"] == "online" && !info["pid"].is_null()
    });
    let pid_before = before["pid"]
        .as_u64()
        .unwrap_or_else(|| panic!("an online sheep reports a pid: {before}"));

    write_shep_toml(&dir, "[dog.metrics]\nbind = \"127.0.0.1:19616\"\n");
    std::fs::write(
        dir.path().join("dogs.toml"),
        "[metrics]\nbind = \"127.0.0.1:19617\"\n",
    )
    .unwrap();

    let reloaded = shep(dir.path())
        .arg("daemon")
        .arg("reload")
        .output()
        .unwrap();
    assert_eq!(
        reloaded.status.code(),
        Some(4),
        "InvalidConfig; stderr={}",
        String::from_utf8_lossy(&reloaded.stderr)
    );

    let after = shep(dir.path())
        .arg("--format")
        .arg("json")
        .arg("flock")
        .output()
        .unwrap();
    assert_success(&after);
    let envelope: serde_json::Value = serde_json::from_slice(&after.stdout).unwrap();
    let sheep = &envelope["data"][0];
    assert_eq!(sheep["status"], "online", "still supervised: {sheep}");
    assert_eq!(
        sheep["pid"].as_u64(),
        Some(pid_before),
        "the refusal must happen before anything is signalled: {sheep}"
    );

    graceful_kill(dir.path());
}

/// An env var set on the `shep daemon reload` invocation must not rescue a
/// file that is invalid on its own: a handover successor execs with the old
/// daemon's argv and environment. The variable is set on the child through
/// `Command::env`; mutating this process's own environment is `unsafe` in
/// edition 2024 and the crate forbids unsafe code.
#[test]
fn a_bad_shep_toml_an_env_var_would_rescue_is_still_refused() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();

    let started = shep(dir.path())
        .arg("--format")
        .arg("json")
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("sheep")
        .output()
        .unwrap();
    guard.adopt_home(dir.path());
    assert_success(&started);

    let before = poll_flock(dir.path(), |info| {
        info["status"] == "online" && !info["pid"].is_null()
    });
    let pid_before = before["pid"]
        .as_u64()
        .unwrap_or_else(|| panic!("an online sheep reports a pid: {before}"));

    // Below MIN_CRON_SLEEP (1s): valid TOML, refused only at DaemonConfig's
    // own validation pass.
    write_shep_toml(&dir, "[daemon]\nmax_cron_sleep = \"500ms\"\n");

    let reloaded = shep(dir.path())
        .arg("daemon")
        .arg("reload")
        .env("SHEP_MAX_CRON_SLEEP", "5s")
        .output()
        .unwrap();
    assert_eq!(
        reloaded.status.code(),
        Some(4),
        "InvalidConfig, the env var on this invocation must not rescue a file the          daemon being replaced never saw it against; stderr={}",
        String::from_utf8_lossy(&reloaded.stderr)
    );

    let after = shep(dir.path())
        .arg("--format")
        .arg("json")
        .arg("flock")
        .output()
        .unwrap();
    assert_success(&after);
    let envelope: serde_json::Value = serde_json::from_slice(&after.stdout).unwrap();
    let sheep = &envelope["data"][0];
    assert_eq!(sheep["status"], "online", "still supervised: {sheep}");
    assert_eq!(
        sheep["pid"].as_u64(),
        Some(pid_before),
        "the refusal must happen before anything is signalled: {sheep}"
    );

    graceful_kill(dir.path());
}

/// A script that says which slot it is and which process it is, on every line.
/// `$SHEP_INSTANCE` is injected at the spawn and fixed for the life of the
/// process, so a line naming a slot is the child's claim rather than the
/// shepherd's, which is what makes a slot swap visible.
#[cfg(unix)]
fn write_slot_script(dir: &TempDir) -> PathBuf {
    write_script(
        dir,
        "slot.sh",
        &format!(
            "{}{}while :; do\n  echo \"slot=$SHEP_INSTANCE pid=$$\"\n  sleep 0.2\ndone\n",
            script_header(),
            record_pid_line(dir),
        ),
    )
}

/// Every line of `path` that names a slot, as `(slot, pid)` pairs. Panics on a
/// line it cannot parse: a torn line is the failure these cases look for, and
/// dropping it would turn a lost write into a shorter list.
#[cfg(unix)]
fn slot_lines(path: &Path) -> Vec<(u32, u32)> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(shep_core::logstamp::strip)
        .map(|line| {
            let (slot, pid) = line
                .split_once(' ')
                .unwrap_or_else(|| panic!("a torn line in {}: {line:?}", path.display()));
            let parse = |field: &str, prefix: &str| {
                field
                    .strip_prefix(prefix)
                    .and_then(|rest| rest.parse::<u32>().ok())
                    .unwrap_or_else(|| panic!("a torn line in {}: {line:?}", path.display()))
            };
            (parse(slot, "slot="), parse(pid, "pid="))
        })
        .collect()
}

/// Waits until `path` holds at least `want` slot lines, or
/// [`HANDOVER_DEADLINE`] expires, and returns what it held on the last read.
#[cfg(unix)]
fn poll_slot_lines(path: &Path, want: usize) -> Vec<(u32, u32)> {
    let start = Instant::now();
    loop {
        let lines = slot_lines(path);
        if lines.len() >= want || start.elapsed() >= HANDOVER_DEADLINE {
            return lines;
        }
        std::thread::sleep(FLOCK_POLL_INTERVAL);
    }
}

/// Polls `path` until the lines written after the first `before` of them
/// satisfy `ready`, or the handover deadline passes.
///
/// A line count is the wrong wait for a `merge_logs` app: both instances write
/// to one file, so "two more lines" is satisfied by either of them writing
/// twice. The caller therefore says what it is waiting for.
#[cfg(unix)]
fn poll_fresh_lines(
    path: &Path,
    before: usize,
    ready: impl Fn(&[(u32, u32)]) -> bool,
) -> Vec<(u32, u32)> {
    let start = Instant::now();
    loop {
        let lines = slot_lines(path);
        if lines.len() > before && ready(&lines[before..]) {
            return lines;
        }
        if start.elapsed() >= HANDOVER_DEADLINE {
            return lines;
        }
        std::thread::sleep(FLOCK_POLL_INTERVAL);
    }
}

/// A clustered app is carried, and every instance comes back in its own slot.
///
/// Two apps, because `merge_logs` points every instance at one log file and
/// `handover::adopt::refuse_repeated_fds` refuses the entire blob when a
/// descriptor number appears twice. A slot swap leaves two live processes
/// adopted under each other's names, so each row's `out_file` is read back and
/// its lines have to agree with the row about slot and pid.
#[cfg(unix)]
#[test]
fn a_clustered_flock_keeps_every_pid_and_every_slot_across_a_daemon_reload() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_slot_script(&dir);
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"split\"\nscript = '{}'\ninstances = 2\n\n\
             [[app]]\nname = \"merged\"\nscript = '{}'\ninstances = 2\nmerge_logs = true\n",
            script.display(),
            script.display(),
        ),
    );
    let mut guard = DaemonGuard::default();

    let started = shep(dir.path())
        .arg("start")
        .arg(&flockfile)
        .output()
        .unwrap();
    guard.adopt_home(dir.path());
    assert_success(&started);

    let before = poll_flock_data(dir.path(), FLOCK_DEADLINE, |data| {
        data.as_array().is_some_and(|rows| {
            rows.len() == 4
                && rows
                    .iter()
                    .all(|row| row["status"] == "online" && !row["pid"].is_null())
        })
    });
    let rows_before = rows_by_slot(&before);
    assert_eq!(
        rows_before.len(),
        4,
        "two apps at two instances each: {before}"
    );
    // The fixture check: `merge_logs` collapsing both instances onto one path
    // is the premise of half this case.
    assert_eq!(
        rows_before[&("merged".to_owned(), 0)].1,
        rows_before[&("merged".to_owned(), 1)].1,
        "merge_logs must really point both instances at one file"
    );
    assert_ne!(
        rows_before[&("split".to_owned(), 0)].1,
        rows_before[&("split".to_owned(), 1)].1,
        "without merge_logs each instance must have its own file"
    );
    for ((name, slot), (_, out_file)) in &rows_before {
        assert!(
            !poll_slot_lines(out_file, 1).is_empty(),
            "{name}:{slot} must be logging before the reload"
        );
    }
    let reloaded = shep(dir.path())
        .arg("daemon")
        .arg("reload")
        .output()
        .unwrap();
    assert_success(&reloaded);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&reloaded.stdout),
        String::from_utf8_lossy(&reloaded.stderr)
    );
    // The exact sentence `handover::RefusedReason`'s `Display` ends with. A
    // looser probe would pass whether or not the reload was refused.
    assert!(
        !text.contains("falls back to a stop-and-start"),
        "a clustered flock is carried now, not refused: {text}"
    );

    let after = poll_flock_data(dir.path(), FLOCK_DEADLINE, |data| {
        data.as_array().is_some_and(|rows| {
            rows.len() == 4
                && rows
                    .iter()
                    .all(|row| row["status"] == "online" && !row["pid"].is_null())
        })
    });
    let rows_after = rows_by_slot(&after);
    assert_eq!(
        rows_after, rows_before,
        "every instance keeps its pid and its own log file: {after}"
    );

    // The mark is taken after the reload returned, not before it was issued. A
    // pid is carried across a handover, so an early mark lets a pre-reload line
    // satisfy "this instance wrote again".
    let counts_before: HashMap<(String, u32), usize> = rows_after
        .iter()
        .map(|(key, (_, out_file))| (key.clone(), slot_lines(out_file).len()))
        .collect();

    // The slot assertion, the one a pid check cannot make. Each row is asked
    // for its own file, and every line written into that file after the
    // reload has to name that row's slot and that row's pid.
    for ((name, slot), (pid, out_file)) in &rows_after {
        let before = counts_before[&(name.clone(), *slot)];
        // A split app's file holds nobody else, so two more lines in it are
        // two more lines from this row. A merged app's file holds both
        // instances, so the wait is this row's own pid turning up.
        let lines = if *name == "merged" {
            poll_fresh_lines(out_file, before, |fresh| {
                fresh.iter().any(|(_, line_pid)| line_pid == pid)
            })
        } else {
            poll_fresh_lines(out_file, before, |fresh| fresh.len() >= 2)
        };
        let fresh = lines.get(before..).unwrap_or(&[]);
        assert!(
            !fresh.is_empty(),
            "{name}:{slot} stopped logging across the handover: {} lines, none of them new",
            lines.len()
        );
        if *name == "merged" {
            // One file for both slots, so the row's own lines are the ones
            // carrying its pid. Both must be present, or a handle was lost.
            assert!(
                fresh.iter().any(|(_, line_pid)| line_pid == pid),
                "merged:{slot} wrote nothing after the reload: {fresh:?}"
            );
            for (line_slot, line_pid) in fresh {
                assert_eq!(
                    rows_after[&("merged".to_owned(), *line_slot)].0,
                    *line_pid,
                    "a merged line's slot and pid disagree with the flock: {fresh:?}"
                );
            }
        } else {
            assert!(
                fresh.len() >= 2,
                "{name}:{slot} stopped logging across the handover: {} fresh lines, wanted 2",
                fresh.len()
            );
            for (line_slot, line_pid) in fresh {
                assert_eq!(
                    (*line_slot, *line_pid),
                    (*slot, *pid),
                    "{name}:{slot}'s own log holds another instance's output: {fresh:?}"
                );
            }
        }
    }

    graceful_kill(dir.path());
}

/// `shep flock`'s JSON rows as `(name, instance) -> (pid, out_file)`. Keyed on
/// the pair, since a name matches as many rows as the app has instances.
#[cfg(unix)]
fn rows_by_slot(data: &serde_json::Value) -> BTreeMap<(String, u32), (u32, PathBuf)> {
    data.as_array()
        .unwrap_or_else(|| panic!("flock data is an array: {data}"))
        .iter()
        .map(|row| {
            let name = row["name"]
                .as_str()
                .unwrap_or_else(|| panic!("a row names its app: {row}"))
                .to_owned();
            let instance = u32::try_from(
                row["instance"]
                    .as_u64()
                    .unwrap_or_else(|| panic!("a row names its slot: {row}")),
            )
            .unwrap();
            let pid = u32::try_from(
                row["pid"]
                    .as_u64()
                    .unwrap_or_else(|| panic!("an online row names its pid: {row}")),
            )
            .unwrap();
            let out_file = PathBuf::from(
                row["out_file"]
                    .as_str()
                    .unwrap_or_else(|| panic!("a row names its out file: {row}")),
            );
            ((name, instance), (pid, out_file))
        })
        .collect()
}

/// A `/bin/sh` sheep that signals readiness on fd 3 and answers every shepherd
/// message with the same reply. The `ready` line is what `wait_ready` holds the
/// sheep at `starting` for, the loop is what `shep trigger` gets an answer
/// from, and `read -r line <&3` is a plain blocking read, which a channel that
/// came back non-blocking would break. The reply names the action verbatim:
/// `ActionWaits` correlates on the action name when the app echoes no id.
#[cfg(unix)]
fn write_channel_script(dir: &TempDir) -> PathBuf {
    write_script(
        dir,
        "chatty.sh",
        &format!(
            "{}{}printf '{{\"kind\":\"ready\"}}\\n' >&3\nwhile read -r line <&3; do\n  \
             printf '{{\"kind\":\"action-reply\",\"action\":\"ping\",\"body\":\"pong\"}}\\n' \
             >&3\ndone\n",
            script_header(),
            record_pid_line(dir),
        ),
    )
}

/// Runs `shep trigger chatty ping` and returns the one row's outcome. Asked
/// identically either side of the reload, and the point is that the two
/// answers are the same.
#[cfg(unix)]
fn trigger_ping(home: &Path) -> serde_json::Value {
    let triggered = shep(home)
        .arg("--format")
        .arg("json")
        .arg("trigger")
        .arg("chatty")
        .arg("ping")
        .output()
        .unwrap();
    assert_success(&triggered);
    let envelope: serde_json::Value = serde_json::from_slice(&triggered.stdout)
        .unwrap_or_else(|e| panic!("trigger stdout was not JSON: {e}"));
    envelope["data"][0]["outcome"].clone()
}

/// A sheep's shepherd channel survives `shep daemon reload`, in both
/// directions and against a real app. A socketpair can survive as a number
/// attached to the wrong end, or be adopted with only one of its two pumps
/// rebuilt, and both leave the flock healthy and the pid unmoved. `wait_ready`
/// is on as well as `channel`, so `online` before the reload is itself proof
/// the child's `{"kind":"ready"}` came up the channel.
#[cfg(unix)]
#[test]
fn a_channel_sheep_still_answers_a_trigger_across_a_daemon_reload() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_channel_script(&dir);
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"chatty\"\nscript = '{}'\nchannel = true\nwait_ready = true\n",
            script.display()
        ),
    );
    let mut guard = DaemonGuard::default();

    let started = shep(dir.path())
        .arg("start")
        .arg(&flockfile)
        .output()
        .unwrap();
    guard.adopt_home(dir.path());
    assert_success(&started);

    // `online` rather than `starting`, which only the child's own readiness
    // line over fd 3 can produce.
    let before = poll_flock(dir.path(), |info| {
        info["status"] == "online" && !info["pid"].is_null()
    });
    let pid_before = before["pid"]
        .as_u64()
        .unwrap_or_else(|| panic!("an online sheep reports a pid: {before}"));
    let answered = trigger_ping(dir.path());
    assert_eq!(
        answered["kind"], "replied",
        "the channel must work before the reload, or this case proves nothing: {answered}"
    );

    let reloaded = shep(dir.path())
        .arg("daemon")
        .arg("reload")
        .output()
        .unwrap();
    assert_success(&reloaded);

    let after = poll_flock(dir.path(), |info| {
        info["status"] == "online" && !info["pid"].is_null()
    });
    assert_eq!(
        after["pid"].as_u64(),
        Some(pid_before),
        "a moved pid means the sheep was respawned, which is the stop arm: {after}"
    );

    let still = trigger_ping(dir.path());
    assert_eq!(
        still["kind"], "replied",
        "the successor must reach the same fd 3 the child has had all along: {still}"
    );
    assert_eq!(still["body"], "pong", "{still}");

    graceful_kill(dir.path());
}

/// Rows [`a_flock_of_every_carried_kind_survives_a_daemon_reload`] expects:
/// four single-instance apps and two clustered ones at two instances each.
#[cfg(unix)]
const ROWS_IN_THE_MIXED_FLOCK: usize = 8;

/// Writes one line to `sheep`'s stdin and asserts shep accepted it. `sent`
/// says the bytes reached the pipe, never that the app read them, so the
/// caller still has to look in the sheep's own log for the echo.
#[cfg(unix)]
fn whisper(home: &Path, sheep: &str, line: &str) {
    let sent = shep(home)
        .arg("whisper")
        .arg(sheep)
        .arg(line)
        .output()
        .unwrap();
    assert_success(&sent);
}

/// Reads `path` until it holds a line equal to `want`, or
/// [`HANDOVER_DEADLINE`] expires. Equality rather than `contains`, so a prefix
/// of a longer line cannot answer for the line itself. Returns rather than
/// panicking, so the failure is the caller's own assertion.
#[cfg(unix)]
fn await_log_line(path: &Path, want: &str) -> bool {
    let start = Instant::now();
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if text
            .lines()
            .any(|line| shep_core::logstamp::strip(line) == want)
        {
            return true;
        }
        if start.elapsed() >= HANDOVER_DEADLINE {
            return false;
        }
        std::thread::sleep(HANDOVER_POLL_INTERVAL);
    }
}

/// A `/bin/sh` sheep that waits for `gate` to appear, deletes it, and then
/// grows its resident set past [`BALLOON_BYTES`]. Growing only after the exec
/// makes the breach attributable to the successor's arming, and deleting the
/// gate on the way past makes it happen exactly once.
#[cfg(unix)]
fn write_gated_ballooning_script(dir: &TempDir, name: &str, gate: &Path) -> PathBuf {
    write_script(
        dir,
        name,
        &format!(
            "{header}{pid}while [ ! -f '{gate}' ]; do\n  sleep 0.1\ndone\nrm -f '{gate}'\n\
             s=x\nwhile [ ${{#s}} -lt {BALLOON_BYTES} ]; do s=\"$s$s\"; done\n{sleep}",
            header = script_header(),
            pid = record_pid_line(dir),
            gate = gate.display(),
            sleep = sleep_line(SLOW_SCRIPT_SLEEP_SECS),
        ),
    )
}

/// Every lifecycle extra is armed again by the successor, proved by behaviour
/// rather than by a handle existing. [`ExtrasRegistry::arm`] fans out to five
/// mechanisms across two scopes: sampling, the memory limit and the liveness
/// loop per instance, the cron worker and the filesystem watch per name.
///
/// Every trigger fires after the exec, and that ordering is the case: a watch
/// armed by the predecessor is indistinguishable from one armed by the
/// successor if the file is written before the reload. `control` configures no
/// extra at all, so its `restarts` of 0 says nothing here restarts sheep.
#[cfg(unix)]
#[test]
fn every_lifecycle_extra_is_re_armed_across_a_daemon_reload() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    // Its own tempdir, never `$SHEP_HOME`: every fixture script appends its
    // pid to `<home>/`[`FIXTURE_PIDS`] on each spawn, so a watch rooted at
    // the home would restart on its own sheep's restart, forever.
    let watched = tempfile::tempdir().unwrap();
    let greedy_gate = dir.path().join("greedy.gate");
    let control_gate = dir.path().join("control.gate");
    // A file the probe requires, so the test trips it by deleting and heals it
    // by writing. The opposite polarity races the fixture: a script clearing
    // its own trigger on the way up can do so after `shep flock` already called
    // the sheep `online`.
    let healthy = dir.path().join("probe.healthy");
    std::fs::write(&healthy, "ok").unwrap();
    let sleeper = write_slow_script(&dir);
    let greedy = write_gated_ballooning_script(&dir, "greedy.sh", &greedy_gate);
    let control = write_gated_ballooning_script(&dir, "control.sh", &control_gate);
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"watched\"\nscript = '{sleeper}'\ncwd = '{root}'\nwatch = true\n\n\
             [[app]]\nname = \"scheduled\"\nscript = '{sleeper}'\ncron_restart = \"* * * * *\"\n\n\
             [[app]]\nname = \"greedy\"\nscript = '{greedy}'\nmax_memory = \"{BREACH_LIMIT}\"\n\n\
             [[app]]\nname = \"probed\"\nscript = '{sleeper}'\n\
             liveness_probe = {{ kind = \"exec\", target = \"test -f {healthy}\", \
             interval = \"1s\", timeout = \"2s\", failure_threshold = 2 }}\n\n\
             [[app]]\nname = \"control\"\nscript = '{control}'\n",
            sleeper = sleeper.display(),
            root = watched.path().display(),
            greedy = greedy.display(),
            control = control.display(),
            healthy = healthy.display(),
        ),
    );
    let mut guard = DaemonGuard::default();

    let boot = shep(home).arg("start").arg(&flockfile).output().unwrap();
    guard.adopt_home(home);
    assert_success(&boot);

    let before = poll_flock_data(home, FLOCK_DEADLINE, |data| {
        data.as_array().is_some_and(|rows| {
            rows.len() == 5
                && rows
                    .iter()
                    .all(|row| row["status"] == "online" && !row["pid"].is_null())
        })
    });
    let pids_before: BTreeMap<String, u64> = restart_counts(&before)
        .keys()
        .map(|name| {
            (
                name.clone(),
                sheep_named(&before, name)["pid"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        restart_counts(&before),
        BTreeMap::from([
            ("control".to_owned(), 0),
            ("greedy".to_owned(), 0),
            ("probed".to_owned(), 0),
            ("scheduled".to_owned(), 0),
            ("watched".to_owned(), 0),
        ]),
        "precondition: nothing has restarted yet: {before}"
    );

    let reloaded = shep(home).arg("daemon").arg("reload").output().unwrap();
    assert_success(&reloaded);
    let after = poll_flock_data(home, FLOCK_DEADLINE, |data| {
        data.as_array()
            .is_some_and(|rows| rows.len() == 5 && rows.iter().all(|row| row["status"] == "online"))
    });
    // The handover really happened, which every assertion below depends on:
    // a stop-and-start would re-arm everything from a fresh spawn and prove
    // nothing about `install_adopted`.
    for (name, pid) in &pids_before {
        assert_eq!(
            sheep_named(&after, name)["pid"].as_u64(),
            Some(*pid),
            "{name} was respawned, so this reload took the stop arm: {after}"
        );
    }
    // The sampling arm, the cheapest of the five to lose silently. A successor
    // that never called `stats.watch` leaves this null for the life of the
    // daemon while every other column looks right.
    for name in pids_before.keys() {
        assert!(
            !sheep_named(&after, name)["memory_bytes"].is_null(),
            "{name} is no longer sampled after the handover: {after}"
        );
    }

    // Every trigger, fired only now.
    std::fs::write(watched.path().join("app.txt"), "changed").unwrap();
    std::fs::write(&greedy_gate, "go").unwrap();
    std::fs::write(&control_gate, "go").unwrap();
    std::fs::remove_file(&healthy).unwrap();
    // One wait for the three fast arms. The enforcer's ticks are phased off
    // daemon boot rather than off the breach, so the memory limit's worst case
    // is a whole `MEMORY_POLL_INTERVAL` after the resident set moves.
    let fired = poll_flock_data(home, BREACH_DEADLINE, |data| {
        ["watched", "probed", "greedy"]
            .iter()
            .all(|name| sheep_named(data, name)["restarts"].as_u64().unwrap_or(0) >= 1)
    });
    assert!(
        sheep_named(&fired, "watched")["restarts"].as_u64().unwrap() >= 1,
        "a write under the watched tree after the exec must restart the sheep: {fired}"
    );
    assert!(
        sheep_named(&fired, "probed")["restarts"].as_u64().unwrap() >= 1,
        "a liveness probe failing after the exec must restart the sheep: {fired}"
    );
    assert!(
        sheep_named(&fired, "greedy")["restarts"].as_u64().unwrap() >= 1,
        "a resident set crossing max_memory after the exec must restart the sheep: {fired}"
    );
    // Healed now that the restart is on the books, so the probe stops
    // failing and `probed` cannot spend its `max_restarts` while the case
    // waits out the cron minute below.
    std::fs::write(&healthy, "ok").unwrap();

    // The cron worker, and the slow one. A `* * * * *` pattern armed at an
    // arbitrary moment lands uniformly in the minute, so the bound is a minute
    // plus the restart's round trip.
    let cronned = poll_flock_data(home, CRON_DEADLINE, |data| {
        sheep_named(data, "scheduled")["restarts"]
            .as_u64()
            .unwrap_or(0)
            >= 1
    });
    assert!(
        sheep_named(&cronned, "scheduled")["restarts"]
            .as_u64()
            .unwrap()
            >= 1,
        "a cron occurrence after the exec must restart the sheep: {cronned}"
    );
    assert_eq!(
        sheep_named(&cronned, "control")["restarts"].as_u64(),
        Some(0),
        "the control ballooned through the same gate and configures no extra at all; \
         a restart it shares is this case restarting sheep rather than an extra firing: \
         {cronned}"
    );

    // The daemon's own log, the only place the observed resident set and the
    // ceiling it crossed are stated. Read rather than polled:
    // `spawn_extras_reporter` writes the record before it asks for the restart.
    let daemon_log = std::fs::read_to_string(home.join("logs").join("shepd.err.log")).unwrap();
    assert!(
        daemon_log.contains("exceeded its max_memory"),
        "the successor's own log must say why the sheep was restarted: {daemon_log:?}"
    );

    graceful_kill(home);
}

/// `restarts` per sheep name, for the two observations this case compares.
#[cfg(unix)]
fn restart_counts(data: &serde_json::Value) -> BTreeMap<String, u64> {
    data.as_array()
        .unwrap_or_else(|| panic!("flock data is an array: {data}"))
        .iter()
        .map(|row| {
            (
                row["name"].as_str().unwrap().to_owned(),
                row["restarts"].as_u64().unwrap_or(0),
            )
        })
        .collect()
}

/// A sheep already owed a respawn when the shepherd is replaced still gets it.
///
/// `Actor::schedule_restart` spawns a task that sleeps and then sends
/// `Msg::RestartDue`, and that task dies with the process image, while
/// `handle_restart_due` is the only thing that moves a sheep off
/// `WaitingRestart`. The precondition is asserted before and immediately after
/// the reload: if the delay elapsed during `daemon reload`, the predecessor
/// respawned the sheep and the final assertion proves nothing. `steady` never
/// exits, so a moved pid means the stop arm ran.
#[cfg(unix)]
#[test]
fn a_sheep_owed_a_restart_still_gets_one_after_a_daemon_reload() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let steady = write_slow_script(&dir);
    let flapper = write_script(
        &dir,
        "flapper.sh",
        &format!("{}{}exit 1\n", script_header(), record_pid_line(&dir)),
    );
    let flockfile = write_flockfile(
        &dir,
        &format!(
            // Long enough that a loaded runner cannot let the wait expire
            // between the observation below and the reload after it, and short
            // enough that the case then waits it out once.
            "[[app]]\nname = \"flapper\"\nscript = '{flapper}'\nrestart_delay = \"8s\"\n\n\
             [[app]]\nname = \"steady\"\nscript = '{steady}'\n",
            flapper = flapper.display(),
            steady = steady.display(),
        ),
    );
    let mut guard = DaemonGuard::default();

    let boot = shep(home).arg("start").arg(&flockfile).output().unwrap();
    guard.adopt_home(home);
    assert_success(&boot);

    let before = poll_flock_data(home, FLOCK_DEADLINE, |data| {
        sheep_named(data, "flapper")["status"] == "waiting-restart"
            && sheep_named(data, "steady")["status"] == "online"
    });
    assert_eq!(
        sheep_named(&before, "flapper")["status"],
        "waiting-restart",
        "precondition: the sheep must be owed a respawn when the shepherd is replaced: {before}"
    );
    let steady_pid = sheep_named(&before, "steady")["pid"].as_u64().unwrap();

    let reloaded = shep(home).arg("daemon").arg("reload").output().unwrap();
    assert_success(&reloaded);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&reloaded.stdout),
        String::from_utf8_lossy(&reloaded.stderr)
    );
    assert!(
        !text.contains("falls back to a stop-and-start"),
        "a sheep in its restart backoff is carried, not refused: {text}"
    );

    let carried = poll_flock_data(home, FLOCK_DEADLINE, |data| {
        !sheep_named(data, "steady")["pid"].is_null()
    });
    assert_eq!(
        sheep_named(&carried, "steady")["pid"].as_u64(),
        Some(steady_pid),
        "a moved pid means the stop arm ran, which restarts everything: {carried}"
    );
    assert_eq!(
        sheep_named(&carried, "flapper")["status"],
        "waiting-restart",
        "the wait must still have been pending at the exec, or this case proves nothing: \
         {carried}"
    );
    assert_eq!(
        sheep_named(&carried, "flapper")["restarts"].as_u64(),
        Some(0),
        "ditto: the predecessor must not have respawned it first: {carried}"
    );

    let restarted = poll_flock_data(home, RESTARTED_DEADLINE, |data| {
        sheep_named(data, "flapper")["restarts"]
            .as_u64()
            .unwrap_or(0)
            >= 1
    });
    assert!(
        sheep_named(&restarted, "flapper")["restarts"]
            .as_u64()
            .unwrap()
            >= 1,
        "the sheep was left waiting for a timer that died with the exec: {restarted}"
    );

    graceful_kill(home);
}

/// A `/bin/sh` sheep that echoes every line it is whispered, prefixed.
/// `stdin = true` is the only thing that gives a sheep a readable fd 0, and the
/// echo comes back in the sheep's own log, so a pipe that survived as a number
/// attached to the wrong end delivers nothing. No trailing `sleep`: the `read`
/// parks the script for as long as the daemon holds the write end open.
#[cfg(unix)]
fn write_echoing_script(dir: &TempDir) -> PathBuf {
    write_script(
        dir,
        "echoer.sh",
        &format!(
            "{}{}while read -r line; do\n  echo \"heard $line\"\n done\n",
            script_header(),
            record_pid_line(dir),
        ),
    )
}

/// A `/bin/sh` sheep that writes down whatever the shepherd tells it and then
/// exits cleanly.
///
/// For `shutdown_with_message`: the daemon writes down the socket on the stop
/// path, with no reply to correlate. The line in the log is the evidence, and
/// the clean `exit 0` beside it says the message arrived rather than the kill
/// ladder.
#[cfg(unix)]
fn write_farewell_script(dir: &TempDir) -> PathBuf {
    write_script(
        dir,
        "bye.sh",
        &format!(
            "{}{}while read -r line <&3; do\n  echo \"told $line\"\n  exit 0\ndone\n",
            script_header(),
            record_pid_line(dir),
        ),
    )
}

/// Every kind of sheep, in one flock, across one reload.
///
/// One flock rather than five: `handover::adopt::refuse_repeated_fds` refuses
/// the entire blob over one repeated number, and a mixed flock is the only
/// place six kinds of descriptor (two log files, two pipe read ends, a stdin
/// pipe and a socketpair, times eight sheep) are numbered together. Every
/// assertion below is one a pid check cannot make.
#[cfg(unix)]
#[test]
fn a_flock_of_every_carried_kind_survives_a_daemon_reload() {
    let dir = tempfile::tempdir().unwrap();
    let counter = write_counting_script(&dir);
    let slot = write_slot_script(&dir);
    let chatty = write_channel_script(&dir);
    let echoer = write_echoing_script(&dir);
    let bye = write_farewell_script(&dir);
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"counter\"\nscript = '{counter}'\n\n\
             [[app]]\nname = \"echoer\"\nscript = '{echoer}'\nstdin = true\n\n\
             [[app]]\nname = \"chatty\"\nscript = '{chatty}'\nchannel = true\nwait_ready = true\n\n\
             [[app]]\nname = \"bye\"\nscript = '{bye}'\nshutdown_with_message = true\n\n\
             [[app]]\nname = \"split\"\nscript = '{slot}'\ninstances = 2\n\n\
             [[app]]\nname = \"merged\"\nscript = '{slot}'\ninstances = 2\nmerge_logs = true\n",
            counter = counter.display(),
            echoer = echoer.display(),
            chatty = chatty.display(),
            bye = bye.display(),
            slot = slot.display(),
        ),
    );
    let mut guard = DaemonGuard::default();

    let started = shep(dir.path())
        .arg("start")
        .arg(&flockfile)
        .output()
        .unwrap();
    guard.adopt_home(dir.path());
    assert_success(&started);

    // `online` for all seven, which for `chatty` is already an assertion:
    // `wait_ready` holds it at `starting` until the child writes up fd 3.
    let before = poll_flock_data(dir.path(), FLOCK_DEADLINE, |data| {
        data.as_array().is_some_and(|rows| {
            rows.len() == ROWS_IN_THE_MIXED_FLOCK
                && rows
                    .iter()
                    .all(|row| row["status"] == "online" && !row["pid"].is_null())
        })
    });
    let rows_before = rows_by_slot(&before);
    assert_eq!(
        rows_before.len(),
        ROWS_IN_THE_MIXED_FLOCK,
        "six apps, two of them clustered: {before}"
    );

    // Every feature is exercised before the reload too, so a whisper or a
    // trigger that never worked at all does not read as a handover defect.
    let out_file = |name: &str| rows_before[&(name.to_owned(), 0)].1.clone();
    let counter_log = out_file("counter");
    let echoer_log = out_file("echoer");
    let bye_log = out_file("bye");
    let seen_before = counting_lines(&counter_log, 3);
    assert!(
        seen_before.len() >= 3,
        "the counter must be logging before the reload: {seen_before:?}"
    );
    whisper(dir.path(), "echoer", "before");
    assert!(
        await_log_line(&echoer_log, "heard before"),
        "the whisper must reach the sheep before the reload, or this case proves nothing: {}",
        std::fs::read_to_string(&echoer_log).unwrap_or_default()
    );
    let answered = trigger_ping(dir.path());
    assert_eq!(
        answered["kind"], "replied",
        "the channel must work before the reload: {answered}"
    );
    let counts_before: HashMap<(String, u32), usize> = rows_before
        .iter()
        .filter(|((name, _), _)| name == "split" || name == "merged")
        .map(|(key, (_, out_file))| (key.clone(), slot_lines(out_file).len()))
        .collect();

    let reloaded = shep(dir.path())
        .arg("daemon")
        .arg("reload")
        .output()
        .unwrap();
    assert_success(&reloaded);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&reloaded.stdout),
        String::from_utf8_lossy(&reloaded.stderr)
    );
    // The exact sentence `handover::RefusedReason`'s `Display` ends with;
    // without it the case would pass on a stop-and-start.
    assert!(
        !text.contains("falls back to a stop-and-start"),
        "every kind in this flock is carried now, not refused: {text}"
    );

    let after = poll_flock_data(dir.path(), FLOCK_DEADLINE, |data| {
        data.as_array().is_some_and(|rows| {
            rows.len() == ROWS_IN_THE_MIXED_FLOCK
                && rows
                    .iter()
                    .all(|row| row["status"] == "online" && !row["pid"].is_null())
        })
    });
    let rows_after = rows_by_slot(&after);
    assert_eq!(
        rows_after, rows_before,
        "every sheep keeps its pid and its own log file: {after}"
    );

    // The log plane, on a plain sheep.
    let seen = counting_lines(&counter_log, seen_before.len() + 3);
    assert!(
        seen.len() > seen_before.len(),
        "the counter stopped logging across the handover: {seen:?}"
    );
    assert_unbroken_sequence(&seen, "the counter's log across a handover");

    // stdin, which nothing else in this file covers. A fresh line, so a
    // stale `heard before` in the file cannot answer for it.
    whisper(dir.path(), "echoer", "after");
    assert!(
        await_log_line(&echoer_log, "heard after"),
        "the carried stdin pipe must still reach the same child: {}",
        std::fs::read_to_string(&echoer_log).unwrap_or_default()
    );

    // The channel, both directions, against the child that has had fd 3
    // since before the exec.
    let still = trigger_ping(dir.path());
    assert_eq!(
        still["kind"], "replied",
        "the successor must reach the same fd 3 the child has had all along: {still}"
    );
    assert_eq!(still["body"], "pong", "{still}");

    // The clustered halves. Each row is asked for its own file, and every
    // line written into it after the reload has to agree with the row.
    for ((name, slot), (pid, out_file)) in &rows_after {
        if name != "split" && name != "merged" {
            continue;
        }
        let before = counts_before[&(name.clone(), *slot)];
        let want = before + 2;
        let lines = poll_slot_lines(out_file, want);
        assert!(
            lines.len() >= want,
            "{name}:{slot} stopped logging across the handover: {} lines, wanted {want}",
            lines.len()
        );
        for (line_slot, line_pid) in &lines[before..] {
            assert_eq!(
                rows_after[&(name.clone(), *line_slot)].0,
                *line_pid,
                "a {name} line's slot and pid disagree with the flock: {lines:?}"
            );
            if name == "split" {
                assert_eq!(
                    (*line_slot, *line_pid),
                    (*slot, *pid),
                    "{name}:{slot}'s own log holds another instance's output: {lines:?}"
                );
            }
        }
    }

    // `shutdown_with_message`, last because it ends its sheep. The message
    // goes down the carried socket, the child writes it to its own log and
    // exits 0.
    let stopped = shep(dir.path()).arg("stop").arg("bye").output().unwrap();
    assert_success(&stopped);
    // The row rides along in the message: a `bye` killed by the stop ladder
    // never got the message, and a `bye` that got it and had the line dropped
    // on the way to the file (what `tokio_runner`'s `FINAL_DRAIN` guards) both
    // leave the log empty.
    assert!(
        await_log_line(&bye_log, "told {\"kind\":\"shutdown\"}"),
        "the stop message must reach the child down the carried channel. \
         The log holds {:?}; the flock reads {}",
        std::fs::read_to_string(&bye_log).unwrap_or_default(),
        poll_flock_data(dir.path(), Duration::ZERO, |_| true),
    );

    graceful_kill(dir.path());
}

/// Gap between the dials [`the_control_socket_accepts_throughout_a_handover`]
/// makes at the control address. A dial is a `connect(2)` and a close, so
/// this decides how narrow an outage the case can see, not what it costs:
/// every real way the address goes away spans a daemon teardown or a fresh
/// bind, hundreds of milliseconds.
#[cfg(unix)]
const DIAL_INTERVAL: Duration = Duration::from_millis(5);

/// `ExitCode::DaemonUnreachable`, the one failing exit `shep ping` has,
/// whatever the reason. Any other failing exit is a usage error or a
/// refusal, which no handover produces, and the prober refuses it rather
/// than counting it as the one drop the exec is allowed.
#[cfg(unix)]
const PING_OFFLINE: i32 = 5;

/// The control socket answers throughout a handover.
///
/// The successor inherits the listening descriptor rather than binding the
/// address again, so a client that connects mid-replacement waits in the
/// kernel's backlog. Nothing may be refused, and the socket file may never
/// disappear: a rebind would race the predecessor's socket file.
///
/// Two probers and one file check. A `connect(2)` dialer sees an outage at
/// least `DIAL_INTERVAL` wide; the socket file's inode sees a rebind, which
/// is too brief for any poller; a ping loop sees a request still served.
/// Ping failures are counted, not read: `shep ping` prints nothing on
/// stderr and exits `DaemonUnreachable` for every reason it has.
#[cfg(unix)]
#[test]
fn the_control_socket_accepts_throughout_a_handover() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();

    let started = shep(dir.path())
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("sheep")
        .output()
        .unwrap();
    guard.adopt_home(dir.path());
    assert_success(&started);
    let _ = poll_flock(dir.path(), |info| info["status"] == "online");

    // One deadline for both threads, so the two answers describe the same
    // window.
    let deadline = Instant::now() + Duration::from_secs(8);
    let socket = dir.path().join("run").join("shep.sock");
    // The file's identity before anything happens to it. A successor that
    // binds fresh instead of adopting must unlink and recreate the file,
    // which changes the inode; that takes a microsecond no poller can see.
    let inode_before = std::fs::metadata(&socket)
        .expect("the control socket must exist before the handover")
        .ino();
    let dial_socket = socket.clone();
    let dialer = std::thread::spawn(move || {
        let mut refused = Vec::new();
        let mut dials = 0_usize;
        while Instant::now() < deadline {
            dials += 1;
            // Dropped where the `if let` ends: the daemon's accept loop meets
            // an EOF and logs it at `debug!`. The answer wanted is the
            // syscall's; anything more would be the bucket this case fixes.
            if let Err(err) = std::os::unix::net::UnixStream::connect(&dial_socket) {
                refused.push(format!("dial {dials}: {:?}: {err}", err.kind()));
            }
            std::thread::sleep(DIAL_INTERVAL);
        }
        (refused, dials)
    });

    let home = dir.path().to_path_buf();
    // The prober says when it is really probing, and the reload waits for
    // that. Without the handshake the reload could finish before the first
    // `ping` ran, and every probe would be served by the successor alone.
    let (probing, started_probing) = std::sync::mpsc::channel();
    let prober = std::thread::spawn(move || {
        let mut before_reload = Vec::new();
        let mut dropped = Vec::new();
        let mut pings = 0_usize;
        let mut announced = false;
        while Instant::now() < deadline {
            pings += 1;
            let out = shep(&home).arg("ping").output().unwrap();
            let served = out.status.success();
            if !served {
                // The failure has to be the one shape a handover can cause.
                // Its reason cannot be read (see the case's doc), but its
                // exit can: anything but `DaemonUnreachable` is a different
                // defect wearing the tolerated drop's clothes.
                assert_eq!(
                    out.status.code(),
                    Some(PING_OFFLINE),
                    "ping {pings} failed for a reason no handover produces: {}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            if !announced {
                // The reload waits for a ping the predecessor ANSWERED. A
                // failure before that has no exec to blame, since none has
                // been asked for, and it must not spend the one drop the
                // exec is allowed: it is kept apart and refused below.
                if served {
                    announced = true;
                    let _ = probing.send(());
                } else {
                    before_reload.push(pings);
                }
                continue;
            }
            if !served {
                dropped.push(pings);
            }
        }
        (before_reload, dropped, pings)
    });
    started_probing
        .recv_timeout(FLOCK_DEADLINE)
        .expect("the prober must reach the shepherd before the reload starts");

    let reloaded = shep(dir.path())
        .arg("daemon")
        .arg("reload")
        .output()
        .unwrap();
    assert_success(&reloaded);
    // The premise, checked. A reload that fell back to stopping and starting
    // really did unbind the address, and the dialer would report that as
    // the defect. Both fallback arms say so on stderr (`commands/daemon.rs`'s
    // two `aside("reload", ...)` calls).
    let reload_aside = String::from_utf8_lossy(&reloaded.stderr);
    assert!(
        !reload_aside.contains("starting one instead")
            && !reload_aside.contains("stopping and starting instead"),
        "this case is about the handover arm and the reload took the other one: {reload_aside}"
    );
    // Same file, same inode: the successor adopted the carried listener. A
    // rebind at the same path passed the dialer 10 of 10; the inode is the
    // deterministic reading of the same property.
    let inode_after = std::fs::metadata(&socket)
        .expect("the control socket must still exist after the handover")
        .ino();
    assert_eq!(
        inode_after, inode_before,
        "the successor bound a fresh listener instead of adopting the carried one: \
         the socket file's inode changed across the handover"
    );

    // The listener's descriptor is carried, so no client ever finds the
    // address unbound; an accepted connection is not, so the one reply in
    // flight at the exec may fail. One at most: the prober is sequential.
    let (refused, dials) = dialer.join().unwrap();
    let (before_reload, dropped, pings) = prober.join().unwrap();
    assert!(
        before_reload.is_empty(),
        "a ping failed before any reload was asked for, at {before_reload:?} of \
         {pings}: the predecessor was not answering, which is not the handover's \
         doing"
    );
    assert!(
        refused.is_empty(),
        "the control address must stay bound across the handover, \
         {} of {dials} dials refused: {refused:?}",
        refused.len()
    );
    assert!(
        dropped.len() <= 1,
        "at most the one request in flight at the exec may drop, got {} of {pings} \
         pings, at {dropped:?}",
        dropped.len()
    );

    graceful_kill(dir.path());
}

/// Reads `roll` until it records exactly `want` apps, or [`FLOCK_DEADLINE`]
/// expires, and returns the bytes it held on the last read.
///
/// The muster roll is written by a debounced task, so "the flock changed" and
/// "the roll on disk says so" are two events, and callers here need the second.
#[cfg(unix)]
fn roll_recording(roll: &Path, want: usize) -> Vec<u8> {
    let start = Instant::now();
    loop {
        let bytes = std::fs::read(roll).unwrap_or_default();
        let apps = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|value| value["apps"].as_array().map(Vec::len));
        if apps == Some(want) || start.elapsed() >= FLOCK_DEADLINE {
            return bytes;
        }
        std::thread::sleep(FLOCK_POLL_INTERVAL);
    }
}

/// A successor that inherited an empty flock must not fall back to the roll.
/// A boot either installs the flock it was handed or restores the roll, and
/// what decides is whether it was handed a flock at all, not how large.
///
/// SIGHUP directly, not `shep daemon reload`, which would start `ghost`
/// through `shep muster` whatever the boot decided. A failed exec leaves no
/// shepherd, so the poll fails on its own `assert_success` and the pid check
/// says a successor answered. The wait is for a sheep to appear, so asserting
/// none did cannot pass by looking too early; it goes through
/// [`poll_flock_data_across_a_handover`] since the raw signal can drop one reply.
#[cfg(unix)]
#[test]
fn a_successor_inheriting_an_empty_flock_does_not_restore_the_roll() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script = write_test_script(&dir);
    let mut guard = DaemonGuard::default();

    let started = shep(home)
        .arg("start")
        .arg(&script)
        .arg("--name")
        .arg("ghost")
        .output()
        .unwrap();
    guard.adopt_home(home);
    assert_success(&started);
    let _ = poll_flock(home, |info| info["status"] == "online");
    let shepherd = wait_for_daemon_pid(home).expect("the shepherd must record a pid");

    assert_success(&shep(home).arg("save").output().unwrap());
    let roll = home.join("flock.json");
    let stale = roll_recording(&roll, 1);
    assert!(
        !stale.is_empty(),
        "the roll must record `ghost` before it can go stale: {}",
        String::from_utf8_lossy(&stale)
    );

    assert_success(&shep(home).arg("delete").arg("ghost").output().unwrap());
    let emptied = poll_flock_data(home, FLOCK_DEADLINE, |data| {
        data.as_array().is_some_and(Vec::is_empty)
    });
    assert_eq!(
        emptied.as_array().map(Vec::len),
        Some(0),
        "the delete must leave an idle shepherd: {emptied}"
    );
    // Waited out rather than assumed: the debounced writer is about to record
    // the empty flock, and a stale roll put back before that write lands would
    // be overwritten by it.
    let _ = roll_recording(&roll, 0);
    std::fs::write(&roll, &stale).unwrap();

    nix::sys::signal::kill(shepherd, nix::sys::signal::Signal::SIGHUP).unwrap();

    let after = poll_flock_data_across_a_handover(home, FLOCK_DEADLINE, |data| {
        data.as_array().is_some_and(|rows| !rows.is_empty())
    });
    assert_eq!(
        after.as_array().map(Vec::len),
        Some(0),
        "a successor must install the flock it was handed and nothing else; \
         this one restored a stale roll: {after}"
    );
    assert_eq!(
        wait_for_daemon_pid(home),
        Some(shepherd),
        "the shepherd must have been replaced in place; a moved pid means \
         SIGHUP stopped it and the polling above started a fresh one, which \
         is not the boot this test is about"
    );

    graceful_kill(home);
}

/// `shep add` registers a sheep, starts nothing, and a later `shep start`
/// brings that same sheep up. Without it, `shep start Flockfile.toml` on a
/// template shipping `env = { DB_HOST = "", DB_PASSWORD = "" }` spawns against
/// an empty database URL and spends the restart budget before it can be
/// configured.
///
/// The listing is read once, not polled: `Request::Add` is answered after the
/// actor has registered, so a build routing `add` through the start path
/// reports `online` on this first read.
#[test]
fn add_registers_a_stopped_sheep_that_a_later_start_brings_up() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script = write_test_script(&dir);
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"pending\"\nscript = '{}'\n",
            script.display(),
        ),
    );
    let mut guard = DaemonGuard::default();

    let added = shep(home)
        .arg("--format")
        .arg("json")
        .arg("add")
        .arg(&flockfile)
        .output()
        .unwrap();
    guard.adopt_home(home);
    assert_success(&added);

    let envelope: serde_json::Value = serde_json::from_slice(&added.stdout)
        .unwrap_or_else(|e| panic!("add stdout was not JSON: {e}"));
    assert_eq!(
        envelope["command"], "add",
        "the envelope names the verb the operator typed: {envelope}"
    );
    assert_eq!(
        envelope["data"][0]["status"], "stopped",
        "registered, not started: {envelope}"
    );
    assert!(
        envelope["data"][0]["pid"].is_null(),
        "a sheep that was never spawned has no pid: {envelope}"
    );

    // The script appends its pid to this file on every run, so an empty one is
    // the child's own evidence that it never executed. Unix only, because
    // `record_pid_line` writes nothing on Windows.
    #[cfg(unix)]
    assert!(
        !dir.path().join(FIXTURE_PIDS).exists(),
        "the script never ran, so it never recorded a pid"
    );

    // Registered rather than merely reported: a row `shep flock` cannot see is
    // not a flock member.
    let listed = shep(home)
        .arg("--format")
        .arg("json")
        .arg("flock")
        .output()
        .unwrap();
    assert_success(&listed);
    let flock: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(flock["data"][0]["name"], "pending", "it is in the flock");
    assert_eq!(flock["data"][0]["status"], "stopped", "and still at rest");

    // By name: a name reads no file, so this can only reach a sheep the flock
    // already holds.
    let started = shep(home).arg("start").arg("pending").output().unwrap();
    assert_success(&started);
    let running = poll_flock(home, |info| info["status"] == "online");
    assert_eq!(
        running["status"], "online",
        "the registered sheep came up: {running}"
    );

    graceful_kill(home);
}

/// `shep add` with no target and no Flockfile in the current directory is a
/// usage error, where bare `shep start` brings a shepherd up: `start`'s
/// empty-directory case means "give me a shepherd with nothing running yet",
/// and `add` produces a shepherd holding nothing either way. The temporary
/// directory is the working directory as well as the home, so there is no
/// Flockfile to discover.
#[test]
fn add_with_nothing_to_add_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();

    let output = shep(home).arg("add").current_dir(home).output().unwrap();

    assert_eq!(
        output.status.code(),
        Some(2),
        "clap's own code for bad arguments: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no target and no Flockfile"),
        "the refusal says what was missing: {stderr}"
    );
    assert!(
        !home.join("run").join("shep.sock").exists(),
        "and no shepherd was started to answer a request nobody made"
    );
}

/// `$SUDO_USER` names `nobody` (passwd home `/var/empty` on macOS,
/// `/nonexistent` on Linux) and `$HOME` is a throwaway standing in for
/// root's. The refusal must name nobody's `~/.shep`, not `$HOME/.shep`.
/// Skipped as root: a broken gate would really install a unit.
#[cfg(unix)]
#[test]
fn a_sudo_startup_without_home_carries_the_target_users_home_not_this_processes() {
    if nix::unistd::geteuid().is_root() {
        eprintln!("skipping: as root this would really install a unit if the gate were broken");
        return;
    }
    let Ok(Some(nobody)) = nix::unistd::User::from_name("nobody") else {
        eprintln!("skipping: no `nobody` user to stand in for $SUDO_USER");
        return;
    };
    let fake_root_home = TempDir::new().unwrap();

    let output = Command::cargo_bin("shep")
        .unwrap()
        .env("HOME", fake_root_home.path())
        .env("SUDO_USER", "nobody")
        .env_remove("SHEP_HOME")
        .arg("startup")
        .arg("--init")
        .arg("systemd")
        .timeout(CMD_TIMEOUT)
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let target_home = nobody.dir.join(".shep");
    let refusal = format!(
        "error[usage]: no directory at {}; create it first (any shep verb run as nobody \
         creates that user's own ~/.shep), or pass --home with the $SHEP_HOME this unit \
         should carry",
        target_home.display()
    );
    assert!(
        stderr.lines().any(|line| line == refusal),
        "the refusal names nobody's own home and both ways out: {stderr}"
    );
    assert!(
        !stderr.contains(fake_root_home.path().to_str().unwrap()),
        "and never this process's $HOME: {stderr}"
    );
    assert!(
        !fake_root_home.path().join(".shep").exists(),
        "nothing is created under a $HOME that is not the target user's"
    );
}
