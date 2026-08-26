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
//! Cases 14 and 15 are the file's slow ones and the reason it no longer
//! finishes in about eleven seconds: a cron occurrence and a memory-limit
//! breach are both events on *real* wall clock — a minute boundary and a
//! 15-second sampling tick — with no seam this tier could pause. Each names
//! its own measured cost; [`CRON_DEADLINE`] carries the argument for spending
//! it rather than marking them `#[ignore]`.
//!
//! Every case's command chain carries `.timeout(CMD_TIMEOUT)` before
//! `.output()`, so a regression that hangs (case 7's `--no-follow`
//! following forever being the live hazard) fails as a named assertion
//! instead of a killed CI job. Every case that can leave a daemon behind
//! registers its `$SHEP_HOME` with a [`DaemonGuard`] immediately after the
//! `Output` that might have spawned one, before any assertion that could
//! panic.

#![cfg(unix)]

use std::io::Read;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use assert_cmd::cargo::CommandCargoExt as _;
use tempfile::TempDir;

/// Bound on every `shep` invocation in this file. `assert_cmd`'s
/// `.output()` blocks unbounded without it; case 7 (`bleats --no-follow`)
/// is the live hazard, since its regression mode is following forever.
///
/// Must outlive [`shep_client::spawn::SPAWN_DEADLINE`], not merely equal it
/// (whole-branch review item 5): the autostart path
/// (`shep_client::spawn::probe_until_ready`, `spawn.rs:298`->`:328`) can run
/// right up to that whole budget before it ever reports
/// `DaemonUnreachable`, plus this binary's own write-and-exit overhead on
/// top — roughly 35s end to end. A `CMD_TIMEOUT` merely equal to
/// `SPAWN_DEADLINE` races `assert_cmd`'s own kill against that report; on a
/// loaded machine the kill can win, and this harness would then observe a
/// killed process instead of the exit-5 failure it meant to exercise. The
/// extra margin below is headroom for that overhead, not a second deadline
/// — expressed as an offset from `SPAWN_DEADLINE` rather than a bare number
/// so the relationship stays visible if either budget ever moves.
const CMD_TIMEOUT: Duration =
    Duration::from_secs(shep_client::spawn::SPAWN_DEADLINE.as_secs() + 15);

/// Bound on how long [`concurrent_cold_starts_produce_exactly_one_daemon`]
/// waits for one of its two racers, after which the case FAILS.
///
/// [`CMD_TIMEOUT`] does not cover this, and the gap is not academic — it is
/// the one that let a real daemon bug stall the suite for minutes at a time
/// rather than report anything. `assert_cmd`'s timeout bounds the *process*
/// wait; the reader threads it joins afterwards are bounded only by EOF on
/// the child's stdout and stderr, and EOF waits for the last copy of the
/// write end to close — including a copy held by a daemon that inherited it
/// (`shep-cli/src/launch.rs`'s `seal_inherited_fds`). A racer that never
/// comes back has to be given up on from out here, by the only thread that
/// can still fail the case.
///
/// Sized off [`CMD_TIMEOUT`] the way that constant is sized off
/// `SPAWN_DEADLINE`: strictly longer, so a racer that is merely slow (a
/// loaded machine, the full autostart budget) still reports its own outcome
/// and this bound only ever fires on a racer that is genuinely stuck.
const RACER_DEADLINE: Duration = Duration::from_secs(CMD_TIMEOUT.as_secs() + 15);

/// How long [`bleats_no_follow_until_written`] keeps retrying.
const BLEATS_DEADLINE: Duration = Duration::from_secs(10);

/// Gap between [`bleats_no_follow_until_written`]'s retries.
const BLEATS_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long a fixture sheep's script sleeps after writing whatever it
/// writes. Long enough that none of the cases using it could plausibly
/// outlast it (each finishes in well under a second of real daemon/sheep
/// work); short enough that a sheep the [`DaemonGuard`] sweep somehow missed
/// self-terminates quickly rather than lingering for the rest of a CI job.
///
/// The two real-clock cases at the bottom of this file are the exception —
/// they wait on wall-clock schedules measured in tens of seconds — and use
/// [`SLOW_SCRIPT_SLEEP_SECS`] instead.
const SCRIPT_SLEEP_SECS: u32 = 60;

/// [`SCRIPT_SLEEP_SECS`] for the two real-clock cases, whose own deadlines
/// run to [`CRON_DEADLINE`].
///
/// Sized to outlast the longest of those deadlines by a wide margin, and that
/// margin is load-bearing rather than slack: it is what lets each of those
/// cases claim the restart it observed came from the trigger under test. A
/// script that could reach its own exit inside the observation window would
/// make "the sheep restarted" equally consistent with a crash loop, and no
/// assertion on the count could tell the two apart. Every second of it is
/// also the exposure a sheep the [`DaemonGuard`] sweep missed would linger
/// for, so it is twice the deadline rather than ten times it.
const SLOW_SCRIPT_SLEEP_SECS: u32 = 300;

/// Basename, under a case's own `$SHEP_HOME`, of the file every fixture
/// script appends its own pid to. See [`record_pid_line`] for why, and
/// [`DaemonGuard`] for who reads it.
const FIXTURE_PIDS: &str = "fixture.pids";

/// How long [`DaemonGuard::drop`] keeps retrying for a parseable daemon pid
/// before giving up and saying so.
///
/// The window it covers is real rather than theoretical:
/// `PidfileLock::acquire` opens the pidfile with `create(true)` and
/// `truncate(false)` (`shep-daemon/src/boot.rs`), while `record` writes the
/// pid into it only once the control socket is bound — so in a fresh
/// `$SHEP_HOME`, which is every case here, the file exists and is *empty* for
/// the whole bind. A case that panics inside that window would otherwise hand
/// this guard an unparseable pidfile and get silence.
const GUARD_PID_DEADLINE: Duration = Duration::from_secs(3);

/// Gap between [`GUARD_PID_DEADLINE`]'s and [`GUARD_SWEEP_WINDOW`]'s retries.
const GUARD_PID_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long [`sweep_flock`] keeps re-reading a case's recorded sheep pids
/// before giving up. Covers the gap between a sheep being spawned (which is
/// when `shep start` reports it `Online`) and its script's first line actually
/// running, which is when the pid reaches disk. See [`sweep_flock`].
const GUARD_SWEEP_WINDOW: Duration = Duration::from_secs(2);

/// How long [`poll_flock`] keeps asking before returning whatever it last
/// saw.
///
/// One deadline for both directions, deliberately: the case that waits for a
/// watch-triggered restart and the case that waits to be sure a dot-file
/// caused none must wait the *same* length, or the negative case proves only
/// that it looked sooner. Sized against the 500ms `DEFAULT_WATCH_DELAY`
/// debounce plus a spawn and two RPC round trips — roughly an order of
/// magnitude of headroom on an idle machine, which is what a loaded one
/// needs.
const FLOCK_DEADLINE: Duration = Duration::from_secs(10);

/// Gap between [`poll_flock`]'s attempts.
const FLOCK_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How long [`poll_metrics`] keeps retrying a scrape of the metrics dog's
/// `/metrics` endpoint before giving up.
///
/// Covers the same real gap [`FLOCK_DEADLINE`] does for a sheep, one hop
/// further out: `shep enable metrics` returns once the `EnableDog` RPC is
/// accepted, before the daemon has necessarily finished exec'ing `shep dog
/// metrics`, let alone before that process has bound its listener. Sized
/// the same as `FLOCK_DEADLINE` — both wait on one freshly spawned process
/// finishing its own startup, not on anything slower.
const METRICS_SCRAPE_DEADLINE: Duration = FLOCK_DEADLINE;

/// Gap between [`poll_metrics`]'s retries.
const METRICS_SCRAPE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Bound on a single scrape attempt's own I/O, inside [`poll_metrics`]'s
/// outer retry loop — a connect that succeeds against a peer that then
/// never answers (unlikely against this dog, but this is the same belt the
/// dog's own `READ_TIMEOUT` buckles on the server side) must not be able to
/// stall the loop past its own deadline.
const METRICS_SCRAPE_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// How long [`poll_http_get`] keeps retrying a `shep serve` worker before
/// giving up. Same reasoning as [`METRICS_SCRAPE_DEADLINE`]: `shep serve`
/// returning success means the RPC registering the sheep landed, not that
/// the worker has bound its listener yet.
const SERVE_HTTP_DEADLINE: Duration = FLOCK_DEADLINE;

/// Gap between [`poll_http_get`]'s retries.
const SERVE_HTTP_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Bound on a single `shep serve` request's own I/O, inside
/// [`poll_http_get`]'s outer retry loop — the same belt
/// [`METRICS_SCRAPE_READ_TIMEOUT`] buckles for the metrics dog.
const SERVE_HTTP_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Bound [`a_served_sheep_stops_on_sigterm_rather_than_riding_the_ladder_to_sigkill`]
/// asserts `shep stop`'s own wall-clock against.
///
/// Not a race against anything asynchronous: `Command::Stop`'s daemon-side
/// handler (`shep-daemon/src/supervisor.rs`'s `begin_manual`) defers its
/// reply until the matched sheep has actually exited, so `shep stop`'s own
/// elapsed time is a deterministic report of which rung of the kill ladder
/// answered — never a value this test could observe mid-flight. A worker
/// that rides the ladder to `SIGKILL` takes AT LEAST `kill_timeout`
/// (`AppConfig::default`'s 1600ms) by the ladder's own construction
/// (`shep-daemon/src/kill.rs`); a worker that handles `SIGTERM` the way
/// `serve::worker::run` is supposed to (copied from the metrics dog's own
/// handler, Task 6's doc) returns in low tens of milliseconds. This bound
/// sits well inside that 1600ms floor with wide margin on both sides, so it
/// distinguishes the two rather than merely hoping load doesn't intervene.
const SERVE_STOP_DEADLINE: Duration = Duration::from_millis(1000);

/// How long [`a_cron_occurrence_restarts_a_sheep_on_the_real_clock`] waits
/// for its occurrence.
///
/// Five-field cron cannot express anything finer than a minute, so a
/// `* * * * *` pattern armed at an arbitrary moment is up to a full 60s of
/// *real* wall clock from its first occurrence — there is no seam to shorten
/// that, which is the whole point of the case. Two and a half minutes covers
/// two successive occurrences, so a runner loaded enough to miss the first
/// one still has a second to answer with.
///
/// This is the most expensive constant in the file and it is deliberately not
/// hidden behind `#[ignore]`: an ignored test closes no gap. Measured over
/// five runs the case cost 26s to 61s, and the variance is entirely where in
/// the minute the daemon happened to boot. It runs concurrently with the rest
/// of this tier, which finishes in about 11s without it, so it *is* this
/// file's wall clock now — see
/// [`a_cron_occurrence_restarts_a_sheep_on_the_real_clock`] for the numbers.
const CRON_DEADLINE: Duration = Duration::from_secs(150);

/// How long [`a_real_memory_breach_restarts_a_sheep`] waits for its breach.
///
/// The real enforcer samples every `shep_daemon::limits::MEMORY_POLL_INTERVAL`
/// (15s) and its ticks are phased off daemon boot rather than off the sheep,
/// so the worst honest wait is one whole interval after the sheep's resident
/// set crosses its ceiling, plus a kill ladder and a respawn. Four times that
/// is headroom for a loaded runner, not a second schedule.
const BREACH_DEADLINE: Duration = Duration::from_secs(60);

/// How long a string [`write_ballooning_script`] grows, in bytes.
///
/// Measured rather than guessed (macOS, `/bin/sh`): a bare `/bin/sh` sitting
/// in `sleep` holds about 1.2 MB resident, and growing a 16 MiB string takes
/// it to about 166 MB, because the doubling loop's intermediate allocations
/// stay in the shell's heap. The case does not lean on that slack, though —
/// the string *alone* is twice [`BREACH_LIMIT`], so a thriftier `/bin/sh` on
/// some other unix that held the string and nothing else would still breach.
const BALLOON_BYTES: u64 = 16 * 1024 * 1024;

/// The `max_memory` the ballooning sheep is given.
///
/// Well above any plausible bare-shell resident set (1.2 MB measured, see
/// [`BALLOON_BYTES`]) and half the string that script grows, so both halves
/// of the claim — under the ceiling before, over it after — hold with a wide
/// margin rather than on a coin toss.
const BREACH_LIMIT: &str = "8M";

/// The `listen_timeout` [`write_never_ready_flockfile`] gives its sheep.
///
/// Short because the two log-plane cases wait it out twice over, and safe to
/// be short because nothing races it: the sheep never signals at all, so this
/// is a delay before a certainty rather than a window some slower machine
/// could close first. The daemon takes a timed-out `wait_ready` sheep `Online`
/// anyway, which is what makes the elapse observable through `shep flock`
/// rather than only through the record under test.
const NEVER_READY_TIMEOUT: &str = "1s";

/// What [`write_rotating_script`]'s sheep prints before the rotation, and
/// what must end up in the renamed archive rather than in the recreated log.
const ROTATE_BEFORE: &str = "before-the-rotation";

/// What the same sheep prints after it, once the reopen has returned. Its
/// arrival in the recreated file is the whole assertion.
const ROTATE_AFTER: &str = "after-the-rotation";

/// The daemon record the two log-plane cases provoke and then read out of
/// `$SHEP_HOME/logs/shepd.err.log`.
///
/// One owner for the string, because the two cases assert opposite things
/// about the same record — present at the default level, absent above it —
/// and a pair that drifted apart would keep passing while proving nothing.
/// It is `shep-daemon`'s `Actor::handle_ready_result` (`supervisor.rs`) that
/// writes it, at `WARN`.
const READINESS_RECORD: &str = "readiness deadline elapsed";

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
/// A bare `sleep`, deliberately not `exec sleep`: verified empirically (`ps`
/// before/after a real `shep kill`) that a bare trailing `sleep` is a
/// *forked* child of the `/bin/sh` process the daemon actually tracks, in the
/// shell's own process group — the wrapper-script shape real users write.
/// This file used to `exec` into it to work around a daemon bug where the
/// graceful stop signalled only the one recorded pid, killing the shell and
/// orphaning that untracked `sleep` grandchild. The stop now goes to the
/// whole process group (`shep-daemon/src/tokio_runner.rs`'s `signal_group`),
/// so the fork is safe to keep — and keeping it means every case in this file
/// exercises the shape that regressed, over the real CLI, rather than the one
/// shape the bug could not reach.
fn write_test_script(dir: &TempDir) -> PathBuf {
    write_script(
        dir,
        "sheep.sh",
        &format!(
            "#!/bin/sh\n{}sleep {SCRIPT_SLEEP_SECS}\n",
            record_pid_line(dir)
        ),
    )
}

/// Writes a script whose top-level process explicitly backgrounds a
/// `sleep 300` and `wait`s on it — a real forked lamb for
/// [`describe_renders_a_real_sheeps_lamb_tree`], distinct from
/// [`write_test_script`]'s own bare trailing `sleep` (a lamb too, per that
/// function's own doc, but that fact is incidental there rather than the
/// point). `wait` keeps the top-level `sh` alive exactly as long as its
/// forked child, so the daemon's own pid stays the one this test started
/// and stopping it still reaches the lamb through the shared process group.
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
/// `$$` in `/bin/sh` is the pid the daemon tracks and the leader of its own
/// process group (`shep-daemon/src/tokio_runner.rs` spawns every sheep with
/// `process_group(0)`), so `-pid` per recorded line reaches that sheep's
/// lambs too — which is what makes [`DaemonGuard`]'s panic-path sweep able to
/// reap a whole flock the daemon's own kill ladder never got to drive.
///
/// One line per *spawn*, appended rather than overwritten, so a restart adds
/// a row instead of replacing one: the dead pid is an `ESRCH` no-op later,
/// and the live one is the whole point.
///
/// The path is `dir`'s own `$SHEP_HOME`, spelled absolutely, because a
/// script's cwd is the sheep's `cwd` and is not this test's to assume. It is
/// quoted for the same reason `write_script`'s callers never build a path by
/// hand — a tempdir path is not guaranteed free of shell metacharacters.
///
/// The append goes to a file and never to stdout: case 4 compares
/// `bleats --no-follow` byte-for-byte against a committed fixture, and one
/// extra line on the sheep's own stdout would break it.
fn record_pid_line(dir: &TempDir) -> String {
    format!(
        "echo $$ >> \"{}\"\n",
        dir.path().join(FIXTURE_PIDS).display()
    )
}

/// [`write_test_script`] with [`SLOW_SCRIPT_SLEEP_SECS`]' sleep instead of
/// [`SCRIPT_SLEEP_SECS`]', for
/// [`a_cron_occurrence_restarts_a_sheep_on_the_real_clock`].
///
/// That case runs one of these twice over — once as the sheep under test and
/// once as the control beside it — so what it has to be is unremarkable and
/// very long-lived. See [`SLOW_SCRIPT_SLEEP_SECS`] for why the length is what
/// makes the control mean anything. The memory case's control is its own
/// ballooning script rather than this one, for the reason
/// [`write_ballooning_script`] gives.
fn write_slow_script(dir: &TempDir) -> PathBuf {
    write_script(
        dir,
        "slow.sh",
        &format!(
            "#!/bin/sh\n{}sleep {SLOW_SCRIPT_SLEEP_SECS}\n",
            record_pid_line(dir)
        ),
    )
}

/// Writes a script that grows its own resident set past [`BALLOON_BYTES`] and
/// then sleeps for [`SLOW_SCRIPT_SLEEP_SECS`].
///
/// [`a_real_memory_breach_restarts_a_sheep`] runs this as *both* its subject
/// and its control, where the cron case's two sheep share
/// [`write_slow_script`]: a control that did not balloon would leave "the
/// allocation itself killed the shell" as a live alternative explanation for
/// the subject's restart, and ruling that out is the control's whole job.
///
/// The growth is a shell string doubled in place, not a child process that
/// allocates: `$$` — the pid the daemon tracks, arms the enforcer against,
/// and records through [`record_pid_line`] — is the process whose resident
/// set actually moves, so the case exercises the enforcer's reading of a real
/// process table without also depending on its ppid walk finding a lamb.
/// (`shep-daemon`'s `limits::sample` unit tier already owns that walk.)
///
/// Pure shell arithmetic, no `head`/`dd`/`/dev/zero`: `${#s}` and `"$s$s"`
/// are POSIX, so the growth does not vary with which coreutils a platform
/// ships. It costs about a quarter of a second, which is well inside the gap
/// before the enforcer's first sampling tick.
fn write_ballooning_script(dir: &TempDir) -> PathBuf {
    write_script(
        dir,
        "balloon.sh",
        &format!(
            "#!/bin/sh\n{}s=x\nwhile [ ${{#s}} -lt {BALLOON_BYTES} ]; do s=\"$s$s\"; done\n\
             sleep {SLOW_SCRIPT_SLEEP_SECS}\n",
            record_pid_line(dir)
        ),
    )
}

/// Writes a script that emits one marker line on stdout, optionally one on
/// stderr, and then sleeps. Same `0o755` requirement as [`write_test_script`],
/// the same [`record_pid_line`] prologue, and the same forked trailing
/// `sleep`, each for the reason given there.
///
/// `None` writes to stderr not at all — not an empty line. An empty line is
/// still a line: it reaches the err file, `--no-follow` renders it, and
/// case 4's byte-exact fixture gains a second object it did not predict.
///
/// The sleep is what makes the output countable: a script that exits is
/// restarted, and each restart appends another copy of every marker, so a
/// byte-exact fixture would stop being byte-exact after the first respawn.
fn write_logging_script(dir: &TempDir, out_marker: &str, err_marker: Option<&str>) -> PathBuf {
    let mut script = format!("#!/bin/sh\n{}echo '{out_marker}'\n", record_pid_line(dir));
    if let Some(err_marker) = err_marker {
        script.push_str(&format!("echo '{err_marker}' 1>&2\n"));
    }
    script.push_str(&format!("sleep {SCRIPT_SLEEP_SECS}\n"));
    write_script(dir, "logging.sh", &script)
}

/// Writes a script that prints [`ROTATE_BEFORE`], blocks until `gate`
/// exists, prints [`ROTATE_AFTER`], and sleeps.
///
/// One script, and it has to have both halves: a rotation is only observable
/// in what happens to a line written AFTER the rename, and a script that
/// wrote everything up front would leave a reopen that did nothing looking
/// exactly like one that worked. The gate is what makes "after" a fact
/// rather than a timing bet — the test creates it once the reopen has
/// already returned.
///
/// Same `sleep 0.1` as [`write_ready_script`], for the reason given there:
/// POSIX requires only whole seconds, and a `/bin/sleep` that refused the
/// fraction degrades this into a busy-wait rather than a hang.
fn write_rotating_script(dir: &TempDir, gate: &Path) -> PathBuf {
    write_script(
        dir,
        "rotating.sh",
        &format!(
            "#!/bin/sh\n{}echo '{ROTATE_BEFORE}'\n\
             until [ -e \"{}\" ]; do sleep 0.1; done\n\
             echo '{ROTATE_AFTER}'\nsleep {SCRIPT_SLEEP_SECS}\n",
            record_pid_line(dir),
            gate.display(),
        ),
    )
}

/// Writes a script that blocks until `sentinel` exists, then announces
/// readiness on the shepherd channel and sleeps.
///
/// The gate is a file the test creates, never a delay. An app's
/// `listen_timeout` takes a `wait_ready` sheep `Online` on elapse whether or
/// not it ever signalled, so a script that merely slept would give the test a
/// `starting` window bounded above by that timeout — and a window a test has
/// to *race* is a window a loaded runner closes early, reddening the suite
/// with no regression behind it. A sentinel makes the window as wide as the
/// test needs it.
///
/// `>&3` is the fd the runner hands every sheep whose app asks for a
/// shepherd channel, and `{"kind":"ready"}` is the wire string
/// `shep-daemon`'s `ChildMessage::Ready` pins.
///
/// `sleep 0.1` is a fractional interval, which POSIX does not require but
/// both platforms this file compiles on provide. If some `/bin/sleep` ever
/// refuses it the loop degrades to a busy-wait rather than a hang, so the
/// case still passes — it just spins for the moment the gate is shut.
fn write_ready_script(dir: &TempDir, sentinel: &Path) -> PathBuf {
    write_script(
        dir,
        "ready.sh",
        &format!(
            "#!/bin/sh\n{}until [ -e \"{}\" ]; do sleep 0.1; done\n\
             printf '{{\"kind\":\"ready\"}}\\n' >&3\nsleep {SCRIPT_SLEEP_SECS}\n",
            record_pid_line(dir),
            sentinel.display(),
        ),
    )
}

/// Shared write-plus-chmod tail of the script helpers above.
fn write_script(dir: &TempDir, name: &str, contents: &str) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, contents).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

/// Writes a Flockfile whose one app asks for a readiness handshake it never
/// performs, so [`NEVER_READY_TIMEOUT`] elapses and the daemon writes
/// [`READINESS_RECORD`] about it.
///
/// The provocation the two log-plane cases needed, chosen because it was the
/// one actually observed doing the job. The obvious alternative — an
/// unresolvable `watch` root, whose `arm_watch` warning is the record
/// `shep-daemon`'s own unit tier captures — does **not** work from this tier:
/// `assemble` passes an app's `cwd` through to `Command::current_dir`
/// unchanged, so a `cwd` that cannot be canonicalized is a `cwd` the child
/// cannot chdir into, and the sheep comes up `errored` having logged nothing.
///
/// A plain [`write_test_script`] sheep is enough here: `wait_ready` opens the
/// shepherd channel on fd 3 and the script simply never writes to it, which is
/// exactly a real app that was configured for a handshake it does not
/// implement.
fn write_never_ready_flockfile(dir: &TempDir) -> PathBuf {
    let script = write_test_script(dir);
    write_flockfile(
        dir,
        &format!(
            "[[app]]\nname = \"gated\"\nscript = \"{}\"\n\
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

/// Best-effort graceful shutdown, called at the end of a test's own success
/// path.
///
/// Mirrors `shep-daemon`'s own `daemon_e2e.rs` `Fixture::shutdown`
/// precedent: on every success path this makes the trailing `DaemonGuard`
/// Drop a no-op, and `DaemonGuard` only does real work on a panic path this
/// function never reaches. It matters for more than tidiness here —
/// `DaemonGuard`'s flock sweep is deliberately gated on
/// `std::thread::panicking()` (see its own doc), so on this path nothing but
/// `shep kill` reaps the sheep, and what it drives is the daemon's own
/// graceful stop of each one rather than a `SIGKILL`. Verified empirically with
/// `ps`/`kill` against a real daemon: three back-to-back runs of this suite
/// before this helper existed left eight orphaned `sleep` processes behind,
/// one per sheep started; after adding this call at the end of every case
/// that does not already `kill` as its own subject, repeated runs left none.
/// The other half of that original fix — making every script `exec` into its
/// final `sleep` — was a workaround for a daemon bug since fixed at the
/// source, and has been reverted (see [`write_test_script`]).
fn graceful_kill(home: &Path) {
    let _ = shep(home).arg("kill").output();
}

/// Boots a daemon on `dir`'s `$SHEP_HOME` with `env` set on the `shep start`
/// that autostarts it, waits for [`write_never_ready_flockfile`]'s sheep to
/// give up waiting, and hands back the daemon's own log.
///
/// The environment reaches the daemon because `launch::launch_command`
/// deliberately does not `.env_clear()` the re-exec, so `SHEP_LOG_JSON` and
/// `SHEP_LOG_LEVEL` are read by the child that installs the subscriber, not by
/// the parent that spawns it.
///
/// Waiting for `online` is what orders the read: `handle_ready_result` writes
/// [`READINESS_RECORD`] *before* it sets the status, so a sheep observed
/// `online` has already had its record written and there is nothing to poll
/// for — the same ordering argument [`a_real_memory_breach_restarts_a_sheep`]
/// makes about its own record.
///
/// The daemon is killed before the log is returned, so a caller's assertion
/// can panic without leaking a supervisor; its own [`DaemonGuard`] covers a
/// panic inside this helper, before the kill.
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

/// A `$SHEP_HOME` whose daemon *and whole flock* this test is responsible
/// for, reaped on `Drop` even if the test panics before its own assertions
/// run.
///
/// # What the panic path costs, and how this closes it
///
/// SIGKILLing the daemon does not reach a sheep. `shep-daemon`'s
/// `tokio_runner.rs` gives every sheep its own process group, deliberately,
/// so the daemon's own `kill_tree` can target one sheep without also hitting
/// itself — which means a sheep is never in the daemon's group, and the one
/// signal this guard can send the daemon stops at the daemon. On the success
/// path that costs nothing, because [`graceful_kill`] has already driven the
/// daemon's real kill ladder over every sheep. On the *panic* path the case
/// never reaches `graceful_kill`, and every sheep it started keeps running,
/// reparented to init, until its own script exits.
///
/// The sweep below closes that: [`record_pid_line`] has every fixture script
/// append its own pid to `<home>/`[`FIXTURE_PIDS`] as its first act, so the
/// pids are on disk before the daemon has even reported the spawn, and this
/// guard can reach a flock it has no RPC-free way to enumerate.
///
/// Two orderings are load-bearing:
///
/// - **The daemon dies first.** A sheep killed while its supervisor is still
///   running is a sheep the restart brain brings straight back, so a sweep
///   that ran first would kill a flock and hand the daemon a reason to
///   respawn it.
/// - **The sweep runs only while panicking**, exactly as
///   `shep-daemon/tests/real_runner.rs`'s `Reaper` does and for the reason it
///   already states: on the success path `graceful_kill` has proven these
///   pids gone, and signalling a pid the OS may since have recycled is a
///   hazard rather than a safety net.
///
/// `Drop` must not panic — panicking while already panicking aborts the
/// process, taking the rest of the run's output with it — so an unreachable
/// daemon is reported with `eprintln!` rather than asserted.
///
/// # `dog_pids`: the grandchild gap
///
/// A dog is a GRANDCHILD of this test process — the daemon spawns it, not
/// the harness — and `tokio_runner.rs` gives it the same per-child process
/// group a sheep gets (this file's own module doc on why a sheep is never
/// in the daemon's group; `shep-daemon`'s own architecture supervises a dog
/// through that exact code path, no special-casing). So `kill_group_of` on
/// the daemon's own pid, the loop below, never reaches a dog any more than
/// it reaches a sheep — [`sweep_flock`] is what closes that gap for a
/// sheep, off pids its own fixture script records; a dog spawned by `shep
/// dog <name>` writes to no such file, so a case that starts one registers
/// its pid here directly with [`Self::adopt_dog_pid`] instead.
///
/// Swept unconditionally, unlike `sweep_flock`'s panic-only gate: on the
/// success path [`graceful_kill`] has already stopped the dog through the
/// shepherd's own kill ladder — the same one that stops every sheep, since
/// nothing here special-cases a dog — so this is an `ESRCH` no-op there
/// (`kill_group_of`'s own doc), not a second teardown path racing the first.
#[derive(Debug, Default)]
struct DaemonGuard {
    homes: Vec<PathBuf>,
    dog_pids: Vec<nix::unistd::Pid>,
}

impl DaemonGuard {
    /// Register a `$SHEP_HOME` whose daemon this test is responsible for.
    /// Call it on the `Output` — that is, immediately after `.output()` and
    /// BEFORE the assertion on `output.status`, which panics on failure.
    /// Registering after the assertion leaks exactly the daemon the guard
    /// exists to reap, in exactly the case (a failed autostart) where a
    /// leaked daemon is most likely.
    fn adopt_home(&mut self, home: &Path) {
        self.homes.push(home.to_path_buf());
    }

    /// Register a dog's own pid — a grandchild the daemon spawned, whose
    /// process group sits outside the daemon's own and so survives this
    /// guard's ordinary `kill_group_of(daemon_pid)` untouched. See this
    /// struct's own doc on `dog_pids` for why. Call it as soon as the pid
    /// is known, same ordering rule [`Self::adopt_home`] gives.
    fn adopt_dog_pid(&mut self, pid: nix::unistd::Pid) {
        self.dog_pids.push(pid);
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let panicking = std::thread::panicking();
        for home in &self.homes {
            match daemon_pid(home) {
                Some(pid) => kill_group_of(pid),
                // No parseable pid, and the case succeeded: the daemon's own
                // graceful shutdown unlinks the pidfile as its last act
                // (`boot.rs`'s teardown), so this is "already gone" rather
                // than "never wrote one".
                None if !panicking => {}
                // No parseable pid on the panic path: the case may have died
                // inside the empty-pidfile window GUARD_PID_DEADLINE
                // documents, so retry before concluding anything.
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

/// SIGKILLs every process group named in `home`'s [`FIXTURE_PIDS`], resweeping
/// until [`GUARD_SWEEP_WINDOW`] expires.
///
/// The window, not the single read, is the fix. A sheep records its pid as its
/// script's first line, but `shep start` reports `Online` off the *spawn*, not
/// off the child's first executed statement — so a case that panics
/// immediately after `start` returns reaches this code while its sheep is
/// still somewhere between `fork` and `execve`, with an empty (or absent)
/// pid file. Measured, not assumed: the first calibration run of this guard
/// read `pids=[]` and left a live `/bin/sh sheep.sh` reparented to init, with
/// the script on disk and correct. Resweeping catches that sheep the moment
/// it writes.
///
/// Bounded rather than convergent on purpose: "the file stopped growing" is
/// not observable from here (no case tells this guard how many sheep to
/// expect), so a named deadline is the honest stopping rule. It costs nothing
/// on the success path, which never calls this, and on the panic path the run
/// is already red.
///
/// The daemon must already be dead when this runs — see [`DaemonGuard`] on
/// why — since a sheep killed under a live supervisor is a sheep the restart
/// brain brings straight back.
fn sweep_flock(home: &Path) {
    let start = Instant::now();
    loop {
        for pid in recorded_fixture_pids(home) {
            // `-pid`: every recorded pid is a `/bin/sh` that leads its own
            // process group, so this reaches its forked lambs too. Re-signalling
            // one already killed on an earlier pass is an ESRCH no-op.
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

/// One non-blocking attempt at the daemon pid recorded at `home`.
fn daemon_pid(home: &Path) -> Option<nix::unistd::Pid> {
    let text = std::fs::read_to_string(home.join("pids/shepd.pid")).ok()?;
    let raw: i32 = text.trim().parse().ok()?;
    Some(nix::unistd::Pid::from_raw(raw))
}

/// [`daemon_pid`], retried until it answers or [`GUARD_PID_DEADLINE`]
/// expires. A daemon still alive populates the pidfile the moment its
/// `PidfileLock::record` runs; one that never populates it is one that
/// already exited, so the deadline is what separates the two.
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

/// SIGKILLs `pid`'s process group, or `pid` alone if it does not lead one.
///
/// Group, not leader: the daemon's own children are in its group. But only
/// while the daemon really IS its own group leader — signalling `-pid` when
/// it is not reaches somebody else's group, and in a test runner that group
/// contains the harness. Case 1 asserts the leader property holds; this
/// checks it rather than assuming it, because `Drop` also runs on the path
/// where case 1 failed. `ESRCH` from `getpgid` means already reaped: fall
/// back to the leader-only signal, which is a no-op in that case.
fn kill_group_of(pid: nix::unistd::Pid) {
    let target = match nix::unistd::getpgid(Some(pid)) {
        Ok(pgid) if pgid == pid => nix::unistd::Pid::from_raw(-pid.as_raw()),
        _ => pid,
    };
    // ESRCH on an already-reaped daemon is the expected happy path.
    let _ = nix::sys::signal::kill(target, nix::sys::signal::Signal::SIGKILL);
}

/// Every pid a fixture script recorded under `home`, in spawn order.
///
/// A missing file means the case started no sheep — several do not — and an
/// unparseable line is skipped rather than fatal: this runs on a path that
/// is already failing, and the pids either side of it are still worth
/// signalling.
fn recorded_fixture_pids(home: &Path) -> Vec<nix::unistd::Pid> {
    let Ok(text) = std::fs::read_to_string(home.join(FIXTURE_PIDS)) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| line.trim().parse::<i32>().ok())
        .map(nix::unistd::Pid::from_raw)
        .collect()
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

// --- Dogs helpers -----------------------------------------------------

/// A port with nothing on it: bind `:0`, read what the OS chose, release it.
///
/// Same idiom `shep-daemon/tests/daemon_e2e.rs`'s own `free_port` uses, and
/// the same residual risk: a stranger could take the port between the
/// release here and the metrics dog's own bind. That loss is loud rather
/// than quiet — the dog refuses to run and `shep dogs` reports it errored —
/// so a case that hits it fails with a named cause rather than measuring
/// something else silently.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("the OS must have a free loopback port")
        .local_addr()
        .expect("a bound listener has an address")
        .port()
}

/// One attempt at a `GET /metrics` scrape against `addr`, over a plain
/// `std::net::TcpStream` — no HTTP crate anywhere in this workspace
/// (`crate::http`'s own module doc gives the reason), so the client side of
/// this exchange is exactly as hand-rolled as the server side.
///
/// Reads to EOF rather than to a declared `content-length`: the metrics
/// dog's own `handle_connection` answers exactly one request per accepted
/// connection and then drops the stream, so the peer closing *is* the end
/// of the response, and there is no keep-alive loop on the other end to
/// race.
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
/// expires, returning whatever the last attempt saw (`""` if every attempt
/// failed to connect at all). Bounded the same way every other poll in this
/// file is — a scrape target that never comes up must fail as a named
/// assertion on an empty string, never hang the case.
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

/// One attempt at a request against a `shep serve` worker, over a plain
/// `std::net::TcpStream` — the same reasoning [`scrape_metrics`] gives, and
/// safe for the same structural reason: `serve::worker`'s own handler
/// answers `Connection: close` on every reply, so reading to EOF is reading
/// the whole response.
///
/// Returns the status code off the response's first line, and everything
/// after the blank line as the body. Not a real HTTP parser — this file has
/// no HTTP crate in it any more than `serve::worker` does — but every
/// response this tier's own fixtures produce is small enough to fit in one
/// read, sent with no chunking.
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

/// Runs `shep --home <home> flock --format json` until it answers a `pid`
/// for a dog named `name`, or [`FLOCK_DEADLINE`] expires — the same real
/// gap [`poll_flock`] covers for a sheep: `shep enable` returning success
/// means the `EnableDog` RPC landed, not that the daemon's own supervisor
/// loop has already recorded a pid for the child it just spawned.
/// `flock`, not `dogs`, on purpose: `Response::Flock` carries both
/// populations in one array (`emit_flock`'s own doc — `Format::Json`
/// renders it undivided), so this needs no verb of its own to reach a
/// dog's pid, the same way [`poll_flock`] itself needs none to reach a
/// sheep's.
///
/// Panics on expiry rather than returning `None`: every case that calls
/// this already has a `DaemonGuard` in scope to adopt the pid into once it
/// is known, and a `None` here would leave a real, running dog process
/// unregistered with the very guard that exists to reap it — worse than a
/// named panic pointing straight at the cause.
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

/// Runs `shep flock --format json` until `done` accepts the whole `data`
/// array, or `deadline` expires, and returns the last observation either way.
///
/// Polls rather than sleeping once: nothing in this tier is synchronous with
/// the daemon's own work, and every deadline in it is sized for a loaded
/// runner. Returning the last observation instead of panicking on expiry
/// keeps the failure that reaches CI the caller's own assertion, naming the
/// value it wanted and the value it got.
///
/// The deadline is a parameter rather than [`FLOCK_DEADLINE`] outright
/// because the two real-clock cases wait on wall-clock schedules — a minute
/// boundary, a 15-second sampling tick — that it is an order of magnitude too
/// short for. It stays a *named* deadline per caller either way: no case in
/// this file sleeps once and asserts.
///
/// Each attempt carries the same [`CMD_TIMEOUT`] every other command here
/// does, so nothing in the loop can block unbounded.
fn poll_flock_data(
    home: &Path,
    deadline: Duration,
    done: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let start = Instant::now();
    loop {
        let output = shep(home)
            .arg("--format")
            .arg("json")
            .arg("flock")
            .output()
            .unwrap();
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

/// [`poll_flock_data`] for the single-sheep cases: waits [`FLOCK_DEADLINE`]
/// and hands `done` — and the caller — that one sheep's `ProcessInfo` rather
/// than the array around it.
fn poll_flock(home: &Path, done: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
    poll_flock_data(home, FLOCK_DEADLINE, |data| done(&data[0]))[0].clone()
}

/// Runs `shep --home <home> --format json describe <name>` until its lamb
/// tree is non-empty, or [`FLOCK_DEADLINE`] expires, returning the last
/// `Output` either way — the fixture-comparison case's own version of the
/// same wait [`describe_renders_a_real_sheeps_lamb_tree`] already does over
/// the table renderer.
///
/// `describe` walks the live process tree only inside its own handler, so
/// the very first call after `start` races the shell script's own fork: on
/// this test's script, `/bin/sh` has to fork and exec the trailing `sleep`
/// before the walk can see it, and that fork has not necessarily happened
/// yet. Pinning a committed fixture with an empty `lambs` array — this
/// file's original shape — bet on losing that race forever, which is
/// exactly backwards: the sheep always eventually has the lamb, so the
/// fixture should describe the state the sheep reaches, and this is what
/// waits for it rather than sampling whatever the first call happened to
/// catch.
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

/// The `data[]` element named `name`, for the two cases that run a control
/// sheep beside the one under test.
///
/// By name rather than by index: the control exists to be read in the same
/// observation as the subject, and `data[0]`/`data[1]` would quietly swap
/// meanings if the daemon's id ordering or a Flockfile's app order ever
/// moved.
fn sheep_named<'a>(data: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    data.as_array()
        .unwrap_or_else(|| panic!("flock data must be an array: {data}"))
        .iter()
        .find(|info| info["name"] == name)
        .unwrap_or_else(|| panic!("no sheep named {name} in the flock: {data}"))
}

// --- Dog index helpers -------------------------------------------------

/// Serves `response` -- a complete raw HTTP response, status line and all
/// -- once, on an ephemeral loopback port, on a background thread, and
/// returns the `http://` URL to read it from. `SHEP_DOG_INDEX`'s own
/// loopback carve-out (`dog_index::require_secure_url`'s own doc) is what
/// makes this possible at all: this file drives the real `shep` binary as
/// a subprocess, so there is no seam to skip the `https://` check the way
/// `dog_index`'s own unit tests can from inside the crate.
///
/// Blocking `std::net`, not tokio: this file has no async runtime of its
/// own, unlike `dog_index`'s own `serve_index` test harness, which this
/// mirrors in every other respect -- drain the request just enough that
/// the client's write never stalls, write the canned response, close.
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

/// [`serve_raw_response`] wrapping `body` as a well-formed 200 -- the shape
/// every case that serves a real index needs.
fn serve_dog_index(body: &str) -> String {
    serve_raw_response(format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    ))
}

/// A two-entry community index, shaped like the live one's own single
/// entry (`web/public/dogs.json`, the same fixture `dog_index`'s own unit
/// tests build from): Spot, clean; and Rex, whose description carries
/// `\u{1b}[2J` -- a raw screen-clear escape a hostile pull request could
/// add, and the whole reason `dog_index::sanitise` exists. Both are valid
/// entries -- neither is dropped, so `skipped` stays zero and this fixture
/// alone cannot exercise that count.
///
/// Wrapped in the `{"$schema": ..., "version": 1, "dogs": [...]}` object
/// the real `dogs.json` carries -- a bare array here is exactly the shape
/// `the_old_bare_array_format_is_refused` (`dog_index.rs`'s own unit
/// tests) proves `parse_index` now refuses, and this fixture has to stay
/// on the accepted side of that line to test anything else.
fn two_entry_index_json() -> String {
    serde_json::json!({
        "$schema": "https://shep.turtlesocks.dev/dogs.schema.json",
        "version": 1,
        "dogs": [
            {
                "name": "Spot",
                "package": "shep-log-rotate",
                "adopt_as": "log-rotate",
                "description": "Rotates grown log files and asks the shepherd to reopen them.",
                "repo": "https://github.com/TurtIeSocks/shep-log-rotate",
                "license": "MIT OR Apache-2.0",
                "category": "logs",
                "source": {
                    "kind": "cargo-git",
                    "url": "https://github.com/TurtIeSocks/shep-log-rotate"
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
///
/// `samples` says whether the verb this envelope answers takes a live
/// resource reading — `flock` and `describe` do, every other verb answers
/// with the numbers the actor holds, which are none. It is the whole reason
/// this helper takes the argument: `memory_bytes` is a real reading off the
/// host and cannot be pinned either, but WHETHER it is there is exactly the
/// asymmetry worth asserting, and this is the only tier with a real sheep
/// and a real sampler to assert it against.
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
    // `cpu_percent` is not asserted either way. It needs a periodic baseline
    // to measure against, and whether one has been recorded depends on
    // whether the daemon happened to live through a poll interval before
    // this line ran — a real condition, but a clock race to assert on.
    info["pid"] = serde_json::Value::Null;
    info["uptime_ms"] = serde_json::Value::Null;
    info["out_file"] = serde_json::Value::Null;
    info["err_file"] = serde_json::Value::Null;
    info["cpu_percent"] = serde_json::Value::Null;
    info["memory_bytes"] = serde_json::Value::Null;
    // `lambs[].pid` races the same way the fields above do — the process
    // table pid `describe`'s own walk found — so it is nulled the same way.
    // `lambs[].name` stays: it names the program the OS reports for that
    // pid, deterministic once the walk has actually caught it (the caller's
    // job, via `poll_describe_lambs`, not this function's).
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
/// normalizes its one `data[]` element (this whole file only ever starts
/// one sheep per `$SHEP_HOME` in these cases), and compares the result
/// against the committed fixture named `command`.
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
/// query happened to reach would only know about its own); and a daemon
/// that inherits a racer's stdout pipe and holds it for life — that racer's
/// `.output()` never returns, and [`RACER_DEADLINE`] fails the case instead
/// of letting it stall. Each racer is collected over a channel rather than
/// by joining its thread for exactly that reason: `JoinHandle::join` has no
/// bounded form, so a stuck racer joined directly stops the suite rather
/// than reporting.
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
            // A closed receiver means the case already gave up on this
            // racer and failed; there is no one left to report to.
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
    // `home` and `socket` are the flock's own paths, which is the point of
    // them being in the envelope at all, and they are a tempdir here. Assert
    // they are right, then null them the same way `pid` is: a fixture cannot
    // hold a path that changes every run.
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
    // `daemon_version` belongs with `pid`, `home` and `socket`: assert it is
    // right, then null it. A fixture that freezes the version turns every
    // release into a red test. release-plz bumps `[workspace.package]` and
    // knows nothing about this file, so its release PR failed on exactly this
    // (v0.1.1, 2026-08-26) and the alpha-to-0.1.0 bump did too. Comparing
    // against the crate's own version still catches the drift worth catching,
    // a ping that stops reporting a version or reports the wrong one.
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

    // A separate home that exists but has never had a daemon: `flock` never
    // autostarts (only `start` does, per `main.rs`), so "nothing is
    // listening" stays true for the whole invocation, unlike `start` against
    // the same path.
    //
    // The directory is created deliberately. An absent `--home` is now its
    // own refusal, asserted just below, so a never-created path would prove
    // the wrong thing here — it would never reach the connect at all.
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

    // And a `--home` naming a directory that is not there is a usage error
    // rather than an unreachable daemon, because there is no flock at that
    // path to be unreachable. The likeliest cause is a typo, and creating it
    // would leave a second empty flock for someone to lose processes in.
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

    // Neither of those homes ever had a daemon (that is the point of both
    // sub-cases) — nothing to gracefully kill there. `home` does.

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

/// `create`-mode rotation through the real binary: rename the live log, run
/// `shep reopen`, and the sheep's next line reaches the recreated path where
/// `shep bleats --no-follow` can print it.
///
/// This is the symptom the verb was built for, end to end. Before it, a
/// rotation left the pump filling the renamed inode: the live path was never
/// recreated, `bleats --no-follow` printed nothing, and it exited 0 while
/// doing so. `daemon_e2e` proves the same swap over the daemon's own socket,
/// but nothing there runs the binary an operator's `postrotate` stanza
/// actually invokes — the argv, the default selector, the exit code and the
/// reading verb are all this tier's to prove.
///
/// Both directions are asserted. That the second line appears rules out a
/// reopen that did nothing; that the first one does NOT rules out a `bleats`
/// that found the archive instead, or a pump that never let go of the old
/// inode — either of which would print both lines and pass a
/// contains-the-marker check on its own.
///
/// The log path is read off `shep flock --format json` rather than derived
/// here, so the test cannot disagree with the daemon about which file it is
/// renaming.
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

    // Through the reading verb rather than the file, so the precondition is
    // the same observation the assertion at the bottom makes.
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

    // The `postrotate` stanza itself: no selector, which is the verb's
    // default and the whole-flock case an operator writes. `--format json`
    // is the one addition, so the envelope's own `command` label is asserted
    // rather than taken on trust: `reopen` and `flush` render an identical
    // `FlockRows` table, so their two labels are swappable with no table, no
    // exit code and no wire request moving at all.
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

    // Only now is the gate opened, so the line below cannot predate the
    // reopen that had to happen for it to land anywhere readable.
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
        std::fs::read_to_string(&archive).unwrap(),
        format!("{ROTATE_BEFORE}\n"),
        "the renamed file must stop growing the moment the handle is swapped"
    );

    graceful_kill(home);
}

/// A rotation that fails, end to end: the whole feedback loop an operator
/// has when `shep reopen` cannot do what it was asked.
///
/// Every other case in this file drives the log plane's happy path. This one
/// drives the chain nothing else touches — a pump that could not open a path
/// again, `SupervisorError::ReopenFailed`, `rpc_error`'s `Internal`, and
/// `ExitCode::Internal`'s 9 — and it is the chain that matters most, because
/// a `postrotate` stanza is a shell script: exit 9 is the entire signal it
/// gets, and a rotation reported as a success leaves that sheep writing one
/// of its streams nowhere while logrotate goes on to compress the archive.
///
/// A directory in stdout's place is the failure with no permission games in
/// it: `open(2)` on a directory fails for every uid, root included, so this
/// cannot pass for the wrong reason on a privileged CI runner. The daemon's
/// own tiers use the same construction one and two layers down.
///
/// stderr's path is left alone, so the message must name stdout's path and
/// only stdout's — a daemon that reported the whole flock, or the wrong
/// path, would still exit 9 and pass a bare status check.
///
// fails if any link in that chain stops carrying the failure: a
// `spawn_reopen_task` that reported a refusing pump as a success, an
// `rpc_error` arm answering a code other than `Internal`, or an
// `ExitCode::from(RpcErrorCode)` that no longer sends `Internal` to 9.
#[test]
fn a_reopen_that_cannot_open_a_path_again_exits_internal() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script = write_logging_script(&dir, "blocked-out-marker", None);
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

    // Read off the daemon's own snapshot rather than derived here, so the
    // test cannot disagree with it about which file it is blocking.
    let online = poll_flock(home, |info| info["status"] == "online");
    let out_file = PathBuf::from(
        online["out_file"]
            .as_str()
            .unwrap_or_else(|| panic!("the daemon reports its own log paths: {online}")),
    );

    // The rotator's rename, and then something in the recreated path's way.
    // Renamed rather than deleted so the pump is in the state a real
    // rotation leaves it in: holding a handle on an inode that answers to a
    // different name, with the live path unopenable.
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
    // Taken from the daemon's own answer rather than assumed to be 0, and
    // asserted as the whole `<name> (id <id>)` prefix — the log path already
    // contains the name, so a bare name check would hold against a message
    // that named no sheep at all.
    assert!(
        message.contains(&format!("blocked (id {})", online["id"])),
        "and the sheep it belongs to: {err}"
    );

    // Out of the daemon's way before the shutdown that follows, so nothing
    // downstream trips over a directory where a log file belongs.
    std::fs::remove_dir(&out_file).unwrap();
    graceful_kill(home);
}

/// `copytruncate`-mode rotation through the real binary: an external rotator
/// copies the live log aside and empties it in place — no `shep` verb, no
/// signal, nothing that tells the daemon it happened — and the sheep's next
/// line lands at offset 0 of that same file.
///
/// The half of external rotation shep does nothing for, which is exactly why
/// it is worth a case: it works only because a log file is opened `O_APPEND`
/// (`shep-daemon`'s `open_append`), so every write seeks to end-of-file
/// atomically and an external truncation moves the next one back to the
/// start without the daemon being told. A handle carrying its own offset
/// instead would put that line past a sparse hole the size of what was
/// emptied, and a weekly rotation would turn a busy sheep's log into a file
/// whose apparent size only ever grows.
///
/// The daemon's unit tier pins the same property against a pump harness, over
/// a handle it reopened. What this adds is the stack an operator actually has
/// — a real daemon, a real spawned sheep, the handle it has held since spawn,
/// and a rotator acting on the file behind both of their backs.
///
/// The file's LENGTH is the whole assertion, for the reason
/// [`flush_empties_a_log_the_sheep_goes_on_appending_to`] gives about its own:
/// `bleats` prints the line either way, since a hole reads back as NUL bytes
/// in front of it, and only the byte count tells an appending handle from a
/// positional one.
// fails if `open_append` stops asking for `.append(true)` — verified by
// replacing it with `.write(true)`, under which this case reports 39 bytes
// where 19 were expected: the hole the first line left, and then the second
// line behind it. Blast radius, measured with `--no-fail-fast`: three cases,
// this one plus `flush_empties_a_log_the_sheep_goes_on_appending_to` in this
// file and `tokio_runner`'s
// `a_reopened_handle_still_appends_so_a_truncation_leaves_no_hole`; every
// other test in the workspace stays green, because a file nobody truncates
// cannot tell the two handles apart.
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

    // Through the reading verb rather than the file, so the first line is
    // known to be on disk before the rotator below copies it.
    let before = bleats_no_follow_until_written(home, &["all"]);
    let printed = String::from_utf8_lossy(&before.stdout);
    assert!(
        printed.contains(ROTATE_BEFORE),
        "precondition: the sheep's first line must be readable before the \
         rotation: stdout={printed}"
    );

    // Read off the daemon's own snapshot rather than derived here, so the
    // test cannot disagree with it about which file this is.
    let online = poll_flock(home, |info| info["status"] == "online");
    let out_file = PathBuf::from(
        online["out_file"]
            .as_str()
            .unwrap_or_else(|| panic!("the daemon reports its own log paths: {online}")),
    );

    // `logrotate copytruncate`, spelled out: copy the file aside, then empty
    // the original in place. Nothing here is a shep verb — the daemon is
    // never told, and the pump goes on holding the same inode at size zero.
    let archive = out_file.with_extension("log.1");
    std::fs::copy(&out_file, &archive).unwrap();
    std::fs::File::create(&out_file).unwrap();
    assert_eq!(
        std::fs::read_to_string(&archive).unwrap(),
        format!("{ROTATE_BEFORE}\n"),
        "sanity: the copy really took the line the truncate is about to drop"
    );
    assert_eq!(
        std::fs::metadata(&out_file).unwrap().len(),
        0,
        "sanity: the truncate really emptied it"
    );

    // Only now is the gate opened, so the line below cannot predate the
    // truncation it has to land after.
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
    // The line above is the only thing the sheep wrote after the truncation,
    // and the loop that read it back has already waited for it to be on disk.
    assert_eq!(
        std::fs::metadata(&out_file).unwrap().len(),
        (ROTATE_AFTER.len() + 1) as u64,
        "the sheep's next line must land at offset 0 of the emptied file: a \
         handle that kept its offset across an external truncation would \
         leave a hole the size of what was emptied in front of it, and \
         `bleats` would print the line just the same"
    );

    graceful_kill(home);
}

/// `shep flush` through the real binary: empty a running sheep's log, and
/// watch it keep logging into the same file afterwards.
///
/// Two properties, and the second is why this reuses the rotating script
/// rather than a simpler one. That [`ROTATE_BEFORE`] is gone proves the
/// truncate happened. That [`ROTATE_AFTER`] — written by the same process,
/// through the same handle the daemon never touched — arrives and is readable
/// proves the handle survived it.
///
/// The file's LENGTH is what proves where that line landed. `O_APPEND` seeks
/// to the end before every write, so an emptied file takes the next line at
/// offset 0; a handle writing at its own preserved offset would put the same
/// line past a sparse hole the size of what was truncated, and the reading
/// verb would print it either way. Only the byte count tells the two apart —
/// as a `contains` check cannot, and as reading the tail cannot, since the
/// hole is behind the bytes it returns.
///
/// `daemon_e2e` proves the path-not-inode rule over the daemon's own socket,
/// but nothing there runs the binary an operator actually types — the argv,
/// the exit code and the reading verb are this tier's to prove.
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

    // Through the reading verb rather than the file, so the precondition is
    // the same observation the assertions below make.
    let before = bleats_no_follow_until_written(home, &["all"]);
    let printed = String::from_utf8_lossy(&before.stdout);
    assert!(
        printed.contains(ROTATE_BEFORE),
        "precondition: the sheep's first line must be readable before the \
         flush: stdout={printed}"
    );

    // Read off the daemon's own snapshot rather than derived here, so the
    // test cannot disagree with it about which file this is.
    let online = poll_flock(home, |info| info["status"] == "online");
    let out_file = PathBuf::from(
        online["out_file"]
            .as_str()
            .unwrap_or_else(|| panic!("the daemon reports its own log paths: {online}")),
    );

    // The selector is explicit because the verb requires one — a bare `shep
    // flush` is a usage error, which the case below pins. `--format json`
    // for the reason the reopen case gives about its own label: the two
    // verbs render the same table, so nothing else here would notice them
    // swapped.
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

    // Only now is the gate opened, so the line below cannot predate the
    // flush it has to survive.
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
    // The line above is the only thing the sheep wrote after the flush, and
    // the loop that read it back has already waited for it to be on disk.
    assert_eq!(
        std::fs::metadata(&out_file).unwrap().len(),
        (ROTATE_AFTER.len() + 1) as u64,
        "the sheep's next line must land at offset 0 of the emptied file: a \
         handle that kept its offset across the truncate would leave a hole \
         the size of what was emptied in front of it, and `bleats` would \
         print the line just the same"
    );

    graceful_kill(home);
}

/// The two halves of `shep flush` reach exactly one target each, asserted in
/// the order that makes both facts stand: the flock half first, while the
/// shepherd's own logs still hold a marker only this test wrote.
///
/// Rin's requirement was that a flock flush never reach the shepherd's own
/// logs without being named. That already held by construction — the daemon
/// inherits those two files as fds 1 and 2 and has no path for a selector to
/// match — but "by construction" is exactly the kind of claim a later
/// refactor falsifies quietly, and `--daemon` is the door it now has to stay
/// on the other side of. So both directions are pinned: `flush all` leaves
/// the marker byte-for-byte, and `flush --daemon` leaves the sheep's own log
/// untouched.
///
/// The marker is written through a handle of the test's own. The daemon holds
/// fd 1 open on the same inode and writes nothing to stdout, so nothing races
/// this — and after the `--daemon` flush that descriptor is `O_APPEND`, so
/// whatever it writes next lands at offset 0 rather than past a hole (pinned
/// directly, without a daemon, in `launch.rs`'s own case).
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
    // the case are measured against.
    let before = bleats_no_follow_until_written(home, &["all"]);
    assert!(
        String::from_utf8_lossy(&before.stdout).contains(ROTATE_BEFORE),
        "precondition: the sheep must have logged something to lose"
    );

    const MARKER: &[u8] = b"a line only the shepherd's own log holds\n";
    let shepd_out = home.join("logs/shepd.out.log");
    let shepd_err = home.join("logs/shepd.err.log");
    std::fs::write(&shepd_out, MARKER).unwrap();
    std::fs::write(&shepd_err, MARKER).unwrap();

    let flock_half = shep(home).arg("flush").arg("all").output().unwrap();
    assert_success(&flock_half);
    assert_eq!(
        std::fs::metadata(&out_file).unwrap().len(),
        0,
        "the flock half must still empty the sheep it named"
    );
    // Table mode, deliberately: the paths ride the JSON whatever the table
    // does, so only the default rendering can show this regressing. An
    // operator who ran a flush and was handed STATUS/PID/UPTIME was told
    // nothing about the files it destroyed — which matters most exactly when
    // an `out_file` was mistyped and the emptied path is not a log at all.
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

    // Now the other direction. The sheep is gated shut and has written
    // nothing since the truncate above, so its log staying empty is not what
    // is asserted — its log is refilled first, so "untouched" is a fact with
    // bytes behind it.
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

/// Fails if `shep flush --daemon` grows a daemon round trip — a
/// `connect_client` on its dispatch arm, or a `Request` of its own.
///
/// Emptying the shepherd's own logs is the one flush that must work while the
/// shepherd is down, which is when an operator most often reaches for it: a
/// daemon that filled a disk with its own diagnostics is not answering. The
/// files belong to the CLI (`launch::launch_command` creates them), so there
/// is nothing to ask. That no socket appears is asserted as well as the exit
/// code, because a `connect_or_spawn` on this arm would autostart a daemon in
/// order to be told to do nothing.
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
        !home.join("run/shep.sock").exists(),
        "this verb must not autostart a daemon to empty files the CLI owns"
    );
}

/// Fails if `shep flush` ever runs without a selector.
///
/// The one command in this CLI whose slip of the finger cannot be undone, so
/// it is pinned through the real binary and not only in clap's unit tests: a
/// `default_value` added to the verb would make a bare `shep flush` empty
/// every log file in the flock and exit 0. No daemon is started, because
/// none is needed — clap refuses this before anything connects, and that it
/// never reaches the socket is part of what is being asserted.
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
        !dir.path().join("run/shep.sock").exists(),
        "a usage error must not have autostarted a daemon"
    );
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
/// This case cannot use the [`shep`] helper — it needs `env_remove` and a
/// hand-built argv — but it takes [`CMD_TIMEOUT`] from it rather than naming
/// its own bound. It used to carry an inline `Duration::from_secs(30)`, which
/// is exactly `shep_client::spawn::SPAWN_DEADLINE` and exactly the value
/// `CMD_TIMEOUT`'s own doc exists to forbid: a bound merely *equal* to the
/// autostart budget races `assert_cmd`'s kill against the autostart path's own
/// report, and on a loaded machine the kill wins. When it wins, this CLI dies
/// with a daemon it launched still booting — and that daemon survives, because
/// `probe_until_ready` never kills or waits its child and `launch.rs` gave it
/// its own process group, so `assert_cmd`'s kill reaches the CLI and stops
/// there. `spawn.rs` is deliberately not changed to close that: whether an
/// autostart that exhausts its deadline should kill the daemon it launched is
/// a product question, not a test-tier one. What follows from it is that
/// nothing in `assert_cmd`'s timeout reaps a daemon, so [`DaemonGuard`] is the
/// only thing that can — and the [`graceful_kill`] below is what keeps it from
/// having to.
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
        .timeout(CMD_TIMEOUT) // never block unbounded; see above
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

    graceful_kill(dir.path());
}

// --- Case 9 --------------------------------------------------------------

/// A write under a `watch = true` sheep's `cwd` restarts it: `restarts` goes
/// from 0 to 1.
///
/// The watched tree is its own [`TempDir`], never this case's `$SHEP_HOME`.
/// That separation is load-bearing rather than tidy: every fixture script
/// appends its pid to `<home>/`[`FIXTURE_PIDS`] on each spawn, so a watch
/// rooted at the home would see its own sheep's restart as the next change
/// to restart on, and the case would never stop restarting.
///
/// What a broken implementation this would catch: a watch that was never
/// armed, or armed against the wrong root (`restarts` stays 0 and this fails
/// on the observed value); a `watch = true` that reached the daemon but
/// normalized away (the sheep comes up and nothing ever restarts it); a
/// debounce that swallowed the trailing event of a burst rather than firing
/// after it (same observable).
#[test]
fn a_write_under_a_watched_tree_restarts_the_sheep() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let watched = tempfile::tempdir().unwrap();
    let script = write_test_script(&dir);
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"watcher\"\nscript = \"{}\"\ncwd = \"{}\"\nwatch = true\n",
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

/// A write to a dot-file under the same watched tree restarts nothing — and
/// the watcher was demonstrably alive the whole time it did not.
///
/// [`a_write_under_a_watched_tree_restarts_the_sheep`] alone cannot catch a
/// dropped default-ignore set: it writes a plain file, which triggers either
/// way. This case is the other half, and the second act is what makes its
/// zero mean something. A dot-file, then a full [`FLOCK_DEADLINE`] of nothing
/// happening, would also be what a watcher that was never armed produces —
/// so afterwards it writes a plain file and requires the restart to land.
/// One armed, delivering watcher; two writes; exactly one restart.
///
/// What a broken implementation this would catch: `DEFAULT_IGNORE_GLOBS`
/// dropped or reduced to `**/.git/**` (the dot-file restarts the sheep and
/// the first assertion fails); an ignore set applied to the wrong side of the
/// filter, so *only* ignored paths triggered (the first assertion fails and
/// the second one would too).
#[test]
fn a_write_to_a_dot_file_under_a_watched_tree_restarts_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let watched = tempfile::tempdir().unwrap();
    let script = write_test_script(&dir);
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"watcher\"\nscript = \"{}\"\ncwd = \"{}\"\nwatch = true\n",
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
    // Polls for the restart that must NOT come, for the same deadline the
    // positive case gives the restart that must: `done` never accepts, so
    // this returns on expiry having asked the whole time.
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

/// A `wait_ready` sheep stays `starting` until it writes `{"kind":"ready"}`
/// to the shepherd channel, and only then reads `online`.
///
/// The only tier that exercises the real fd-3 channel end to end: every
/// other test of this gate hands the supervisor a `ChildMessage` directly.
///
/// Two deliberate choices keep it from being a race dressed as a test. The
/// script blocks on a sentinel file rather than a delay (see
/// [`write_ready_script`]), and the app raises `listen_timeout` far above its
/// 3000ms default — because on elapse the daemon takes a `wait_ready` sheep
/// `Online` anyway, so leaving it at the default would make the observation
/// window and the timeout window the same window, and a slow runner would
/// then see `online` for the wrong reason.
///
/// What a broken implementation this would catch: a spawn that ignored
/// `wait_ready` and reported `Online` immediately (the `starting` assertion
/// fails before the sentinel is ever created); a shepherd channel that was
/// never opened on fd 3, or opened and never read (the sheep stays `starting`
/// past the sentinel and the second poll expires); a `Ready` message parsed
/// under a different wire string (same observable, and the byte string here
/// is the one `ChildMessage::Ready` pins).
#[test]
fn a_wait_ready_sheep_goes_online_only_once_it_signals_ready() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let sentinel = dir.path().join("go");
    let script = write_ready_script(&dir, &sentinel);
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"gated\"\nscript = \"{}\"\nwait_ready = true\nlisten_timeout = \"120s\"\n",
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

/// A `cron_restart` pattern that is not a cron pattern is a config error:
/// exit `4`, JSON on stderr, and the offending pattern in the message.
///
/// This and [`an_https_probe_target_is_a_config_error`] are the proof that
/// spec §5's "typos fail loudly at parse time" survives the whole trip —
/// `normalize` rejects the app, the daemon answers `InvalidConfig` over RPC,
/// and the CLI turns that into an exit code. Nothing before this tier spans
/// all three.
///
/// The message is asserted on the presence of the pattern, not on wording:
/// the reason text is croner's and is not ours to pin.
///
/// What a broken implementation this would catch: a `normalize` that
/// validated `cron_restart` only when some other field was set, or not at all
/// (`shep start` exits 0 and the sheep comes up with a schedule that never
/// fires — the silent-failure shape this project keeps rooting out); an RPC
/// layer that mapped `NormalizeError` onto `Internal` or `SpawnFailed`
/// instead of `InvalidConfig` (the exit code is 1 or 6, not 4); a CLI that
/// dropped the daemon's message and substituted its own (the pattern is
/// absent from stderr).
#[test]
fn a_bad_cron_pattern_is_a_config_error() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script = write_test_script(&dir);
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"crony\"\nscript = \"{}\"\ncron_restart = \"not a cron\"\n",
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

/// An `https://` probe target is a config error: exit `4`, JSON on stderr,
/// and the offending target in the message.
///
/// The daemon's HTTP prober is hand-rolled and carries no TLS stack, and a
/// probe that silently failed every poll would look exactly like a down app —
/// so the target is refused at config time instead (decision D1). Same shape
/// and same three-layer reach as
/// [`a_bad_cron_pattern_is_a_config_error`].
///
/// What a broken implementation this would catch: a `ProbeTarget` parser that
/// accepted any URL scheme and left the prober to fail at runtime
/// (`shep start` exits 0, and the app is unreachable in a way indistinguishable
/// from being down); a `normalize` that validated `liveness_probe` but not
/// `readiness_probe`, or the reverse — this case configures the readiness
/// one, and `normalize`'s own unit tier covers the other.
#[test]
fn an_https_probe_target_is_a_config_error() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script = write_test_script(&dir);
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"probed\"\nscript = \"{}\"\n\
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

/// A `* * * * *` occurrence restarts a real sheep on the real system clock:
/// `restarts` goes from 0 to 1 at a wall-clock minute boundary.
///
/// The only place the cron subsystem ever runs on `SystemClock`. Every other
/// cron test drives `TestClock` over a paused runtime, and `SystemClock`
/// itself appears in exactly one dyn-compatibility smoke test that constructs
/// one and never reads it — so "an occurrence fires against real wall time"
/// was, before this case, a claim spec §4 makes and no tier proved. The
/// nearest existing e2e case, [`a_bad_cron_pattern_is_a_config_error`], only
/// ever asserts that a *bad* pattern is rejected, which says nothing about a
/// good one firing.
///
/// `unscheduled` is the control, and it is what rules out the competing
/// explanation. It runs the same script under the same daemon and differs
/// only in configuring no `cron_restart`, so a restart that came from the
/// script exiting and being brought back — a crash loop, not an occurrence —
/// would move both counters rather than one. The script's
/// [`SLOW_SCRIPT_SLEEP_SECS`] sleep is the other half of that argument:
/// it outlasts [`CRON_DEADLINE`] twice over, so neither sheep can reach its
/// own exit inside the window at all.
///
/// Measured cost: 26s, 34s, 42s, 54s and 61s over five runs — a `* * * * *`
/// pattern armed at an arbitrary moment is a uniform draw on the minute it
/// lands in, so the only bound worth stating is the upper one: a minute plus
/// the restart's own round trip. See [`CRON_DEADLINE`] for why that is spent
/// rather than `#[ignore]`d.
///
/// What a broken implementation this would catch: a `SystemClock` that
/// returned a fixed instant instead of reading the clock — the worker parks,
/// wakes, finds `now` still short of `next`, and loops forever while
/// `restarts` stays 0; an `arm_cron` never reached from the real `Online`
/// transition, which no unit tier can see because every one of them arms the
/// registry by hand; a `cron_restart` accepted by `normalize` and then
/// dropped on the way to the daemon (the config-error case above passes
/// either way, since it never gets as far as a schedule that runs).
// fails if `SystemClock::now_utc` stops reading the real clock — verified by
// replacing its body with `DateTime::UNIX_EPOCH`, which reddens this case and
// nothing else in the workspace.
#[test]
fn a_cron_occurrence_restarts_a_sheep_on_the_real_clock() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script = write_slow_script(&dir);
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"minutely\"\nscript = \"{script}\"\ncron_restart = \"* * * * *\"\n\n\
             [[app]]\nname = \"unscheduled\"\nscript = \"{script}\"\n",
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

/// A real RSS breach restarts a real sheep: a script that grows its resident
/// set past its app's `max_memory` is restarted by the real `PollingEnforcer`
/// sampling the real process table.
///
/// The only place `PollingEnforcer` and `SysinfoSampler` run together on real
/// time against a real spawned process. The enforcer's own tier drives it
/// through a `ScriptedSampler` on a paused clock; the registry's tier drives
/// arming through a `RecordingEnforcer` that measures nothing. Neither can
/// see the chain this case does: the actor arming the real enforcer at the
/// `Online` transition, a 15-second sampling tick landing on a process whose
/// resident set really moved, the breach reaching the reporter, and
/// `extra_restart`'s guards letting it through to a real kill ladder and a
/// real respawn.
///
/// `unlimited` is the control, and it is what makes the restart attributable.
/// It runs the *same ballooning script* under the same daemon, grows the same
/// resident set, and differs only in naming no `max_memory` — so a restart
/// caused by the script exiting, or by the shell dying under its own
/// allocation, would move both counters. Only a breach moves exactly one. The
/// script's [`SLOW_SCRIPT_SLEEP_SECS`] sleep is the other half: it outlasts
/// [`BREACH_DEADLINE`] five times over, so neither sheep can reach its own
/// exit inside the window.
///
/// Measured cost: 16s — one `MEMORY_POLL_INTERVAL` plus a restart — bounded
/// by [`BREACH_DEADLINE`]. It runs beside the rest of this tier rather than
/// after it, and the cron case above has never been observed finishing
/// sooner, so it has yet to be what any run of this file waited on.
///
/// It reads the daemon's own log, as cases 16 and 17 do — but it is the only
/// one that reads a record for its *contents* rather than for its presence,
/// and the only one whose record is a consequence of the behaviour under test
/// rather than a provocation staged to produce it. The breach record carries
/// the observed RSS and the ceiling it crossed, which no bus event does, and
/// it is written on this very restart.
///
/// What a broken implementation this would catch: a `SysinfoSampler` that
/// stopped reading the real process table; an `arm_instance` that never
/// reached the real enforcer from the real `Online` transition; a breach that
/// reached the reporter and was logged rather than restarted; an enforcer
/// armed against the sheep's id where its pid belongs, which
/// `extra_restart`'s own pid guard would then silently drop for the whole
/// life of the daemon; and a daemon that renders none of its own records,
/// because no subscriber was installed or its sink was not stderr.
// fails if `SysinfoSampler::sample` stops reading the machine's process table
// — verified by replacing its body with `Vec::new()`, which reddens this case
// plus two unit tests: the sampler's own smoke test, and `extras`'
// `real_extras_wire_the_enforcer_to_the_reports_channel`. Those two are why
// this is the narrower of the two gaps this file closes — the real sampler and
// the real breach channel each already had a unit-tier claim on them. What
// neither asserts, and what nothing asserted before this case, is that a
// breach restarts anything.
#[test]
fn a_real_memory_breach_restarts_a_sheep() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let script = write_ballooning_script(&dir);
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"greedy\"\nscript = \"{script}\"\nmax_memory = \"{BREACH_LIMIT}\"\n\n\
             [[app]]\nname = \"unlimited\"\nscript = \"{script}\"\n",
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

    // The daemon's own log, end to end: `commands::daemon` installs the
    // subscriber on stderr, `launch.rs` redirects the re-exec'd daemon's
    // stderr into this file, and the breach record is the ONLY place the
    // observed RSS and the ceiling it crossed are ever stated — the bus event
    // says `restart` and never why.
    //
    // Read rather than polled: `spawn_extras_reporter` writes the record
    // BEFORE it asks for the restart, so the counter above reaching 1 has
    // already ordered the write ahead of this read.
    //
    // fails if no subscriber is installed at all — a user watching a sheep
    // restart over and over then has nothing, anywhere, telling them it is
    // memory — and fails if the daemon's records stop going to stderr, which
    // is the one sink `launch.rs` captures.
    //
    // It also fails if `main::run`'s `daemon` arm goes back to holding a
    // `stderr().lock()` guard for the daemon's whole life. That guard makes
    // this very record block forever on a worker thread and wedges the
    // daemon, which this case sees as the *next* `shep flock` failing its
    // handshake rather than as an empty file — the wedge is what turned an
    // empty log into a dead supervisor.
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

/// `SHEP_LOG_JSON=1` renders the daemon's own records as JSON — one object per
/// line, in the file `launch.rs` redirects the daemon's stderr into.
///
/// `shep-core` already pins that `SHEP_LOG_JSON` *parses* into
/// `DaemonConfig::daemon::log_json`, and [`a_real_memory_breach_restarts_a_sheep`]
/// already pins that a record reaches `shepd.err.log` at all. Between the two
/// sat the knob's actual job — choosing a renderer — which nothing asserted:
/// dropping the `.json()` call left the whole workspace green while the flag
/// silently did nothing.
///
/// Every non-empty line is parsed, not only the one under test. `log_json`
/// exists so `shepd.err.log` can be read by a machine, and a file where one
/// line in twenty is prose is not that file — the assertion has to be about
/// the stream, not about a record that happens to be well-formed.
///
/// What a broken implementation this would catch: a `log_json` branch that
/// selects the human renderer anyway (no line parses); a subscriber whose
/// records go somewhere other than the stderr `launch.rs` captures (the file
/// is empty); and a `--format json` error envelope torn in half by a record
/// written from a worker thread mid-write, which is the one way this file
/// could gain a line that is *almost* JSON.
// fails if `install_log_subscriber` stops selecting the JSON renderer for
// `log_json` — verified by replacing `builder.json().try_init()` with
// `builder.try_init()`, which reddens this case, and only this case, across
// `cargo test --workspace --all-features`.
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

/// The daemon's own records reach `shepd.err.log` with no ANSI escapes in
/// them.
///
/// `install_log_subscriber` passes `.with_ansi(ansi_enabled(..))`, and
/// `tracing_subscriber`'s own default is colour ON whenever its `ansi`
/// feature is compiled in — it does not consult the terminal by itself. So
/// deleting that one call, or handing it a `true`, fills the daemon's log
/// with escape sequences: unreadable in `less`, and a trap for every
/// substring assertion in this file, since an escape can land in the middle
/// of a field name.
///
/// Asserted on purpose here because it is otherwise pinned only by accident.
/// `a_real_memory_breach_restarts_a_sheep` checks its log for `limit=`, which
/// escapes happen to break — an incidental guard, one rewritten assertion
/// away from being gone, and one that names colour nowhere.
///
// fails if `install_log_subscriber` drops its `.with_ansi(..)` call, or
// passes a constant `true` in place of `ansi_enabled`.
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

/// `SHEP_LOG_LEVEL` decides which of the daemon's records survive: the same
/// `WARN` is written at the default level and filtered out at `error`.
///
/// The env variable, not the `[daemon] log_level` file key it overrides —
/// that is all this body sets, and it is all this file can set: no case here
/// writes a `shep.toml` at all, so every `[daemon]` key reaches the daemon
/// through `SHEP_*` layering or not at all. What that leaves uncovered end to
/// end is the file half of `DaemonConfig` — discovery, parse, and the
/// precedence between a file value and the variable that overrides it — which
/// is pinned in `shep-core`'s own tests and nowhere above them.
///
/// Both halves provoke the identical record on identical configuration, so the
/// only thing that differs between them is the knob — which is what makes the
/// absent half mean "filtered" rather than "never happened". A one-sided case
/// asserting only the absence would pass just as well against a daemon that
/// had stopped writing the record at all, and one asserting only the presence
/// would pass against a hard-coded filter.
///
/// `error` rather than `off` on purpose: `off` also happens to be what an
/// `EnvFilter` built from an empty or unparseable directive degrades toward,
/// so a half that only proved silence would be consistent with the level never
/// having been read. `error` is a level with records above and below it, and
/// the record under test sits on the far side.
///
/// What a broken implementation this would catch: a filter built from a
/// literal instead of from the configured level (the `error` half still logs);
/// a `SHEP_LOG_LEVEL` parsed into config and then never handed to the
/// subscriber, which is the same silent-knob shape `log_json` had (same
/// observable); and a subscriber installed with no filter at all (both halves
/// log, plus every `debug!` in the daemon).
// fails if `install_log_subscriber` stops building its filter from
// `config.daemon.log_level` — verified by replacing
// `EnvFilter::new(config.daemon.log_level.as_str())` with
// `EnvFilter::new("warn")`, which reddens this case, and only this case,
// across `cargo test --workspace --all-features`.
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

/// `shep start <name>` on a sheep that cannot spawn must report the failure
/// the same way `shep start <path>` against the identical broken script
/// does, rather than exiting 0 with nothing on either stream.
///
/// Reproduces the gap Rin found live 2026-08-19: the daemon's
/// `Response::Restarted` (what `shep start <name>` sends once the sheep is
/// already registered — see `lifecycle::resume`) has no per-id error slot,
/// so a respawn that fails to spawn still answers `Ok` with an `errored`
/// row rather than an RPC error (`shep-daemon/src/supervisor.rs`'s
/// `respawn`, `Err` arm). `shep start <path>`'s own `Request::Start` does
/// not share that gap — `do_start` returns `Err(SpawnFailed)` from the
/// identical failure — which is what let the by-name form exit 0 and print
/// nothing while the by-path form against the same script reported
/// `error[spawn_failed]`.
///
/// The script is valid shell but not executable (`0o644`), so every spawn
/// of it fails with `EACCES` regardless of which request registered or
/// restarted it — the same shape Rin's own repro used.
///
/// What a broken implementation would let through: reverting `resume`'s
/// `any_restart_failed` check (`lifecycle.rs`) makes the second `start`
/// below exit `Success` with an empty stderr again, exactly the bug this
/// pins.
#[test]
fn starting_an_errored_sheep_by_name_reports_the_same_failure_as_by_path() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("broken.sh");
    std::fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o644);
    std::fs::set_permissions(&script, perms).unwrap();
    let mut guard = DaemonGuard::default();

    // By path: exit 7 (spawn_failed), stderr names the reason, stdout
    // empty. Also autostarts the daemon the second command below reuses,
    // and registers the sheep this test's second half restarts by name.
    let by_path = shep(dir.path())
        .arg("--format")
        .arg("json")
        .arg("start")
        .arg(&script)
        .output()
        .unwrap();
    guard.adopt_home(dir.path());
    assert_json_error(&by_path, 7, "spawn_failed");

    // The sheep must actually be sitting in the flock as `errored`, or the
    // second command below is not exercising `resume`'s `Request::Restart`
    // path at all — it would fall through to `resolve_target`'s path arm
    // instead, which is the already-working case this test is not about.
    let flock = poll_flock(dir.path(), |info| info["status"] == "errored");
    assert_eq!(
        flock["status"], "errored",
        "the by-path failure must leave the sheep registered as errored: {flock}"
    );

    // By name, same broken script, same failure: must report it exactly
    // like the by-path command above did, not silently succeed.
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

/// `shep restart <name>` must report a respawn that cannot spawn, the same
/// way `shep start` does in both its forms.
///
/// The sibling test above fixed `start <name>`; `restart` reached the same
/// daemon reply through its own handler and kept exiting 0 in silence. The
/// cause is shared: `Response::Restarted` has no per-id error slot, so a
/// spawn that failed comes back as an ordinary `errored` row inside an `Ok`
/// (`shep-daemon/src/supervisor.rs`'s `respawn`, `Err` arm), and a caller
/// that trusts the `Ok` reports success.
///
/// Unlike `start <name>`, `restart` still prints its table: it is a
/// multi-target verb, and an operator restarting a flock wants to see which
/// members came back. What changes is the exit code and the line on stderr.
///
/// The script is valid shell at `0o644`, so every spawn fails `EACCES`. It
/// deliberately has NO extension: `.sh` now maps to `sh` through the
/// starter interpreter mapping, which would run a non-executable file
/// perfectly well and quietly delete this test's whole premise.
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

/// The missing-node sentence, produced for real rather than quoted.
///
/// `deferred.md` recorded this as untestable: producing it needs a `PATH`
/// with no node on it, and `std::env::set_var` is `unsafe` in edition 2024
/// inside a crate that forbids unsafe code. That is true of a UNIT test,
/// which would have to mutate its own process. It is not true here: this
/// tier already runs shep as a subprocess, and `Command::env` sets the
/// CHILD's environment without touching the parent's, so there is nothing
/// unsafe and nothing racy about it.
///
/// `docs/migration.md` quotes this sentence for an operator without node
/// installed. Until now nothing re-checked that quote against the `format!`
/// that produces it, and the two were kept in step by hand.
#[test]
fn a_js_flockfile_without_node_says_so_and_says_what_to_do() {
    let dir = tempfile::tempdir().unwrap();
    let flockfile = dir.path().join("Flockfile.js");
    // Declares a real app, so the ONLY thing that can fail here is the
    // missing interpreter. With node present this Flockfile is valid, which
    // is what makes the assertion below about node rather than about shape.
    std::fs::write(
        &flockfile,
        "module.exports = { app: [{ name: 'web', script: './server.js' }] };\n",
    )
    .unwrap();
    let mut guard = DaemonGuard::default();

    // An empty PATH for the child only. `node` cannot be found, which is the
    // whole condition under test, and the parent's environment is untouched.
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

// --- shep init (lesson 3) ---------------------------------------------------
//
// These fail until the verb exists. `shep init` has no clap subcommand at
// all yet, so every one of them currently dies on "unrecognized subcommand" --
// which is exactly the point: the scaffold functions in
// `crates/shep-cli/src/commands/init.rs` are unreachable from the command
// line, and nothing until now has said so out loud.
//
// They live in the e2e tier rather than beside the functions they exercise
// for two reasons. Writing a file is the behaviour under test, and a
// subprocess is the only place `shep init` can actually be run; and this
// file compiles whether or not the verb exists, so the handoff does not
// depend on any particular shape of the implementation.

/// The plain case: a directory with no Flockfile gets one.
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

/// What it writes must be loadable, not merely present.
///
/// The unit tests already prove the scaffold parses; this proves the bytes
/// that reach disk are the same ones, which is a different claim.
#[test]
fn what_shep_init_writes_is_a_flockfile_shep_can_read() {
    let dir = tempfile::tempdir().unwrap();
    shep(dir.path())
        .current_dir(dir.path())
        .arg("init")
        .output()
        .unwrap();

    // Uncommenting is what makes it a live Flockfile; `shep start` against
    // the file as written would refuse it for declaring no apps, which is
    // its own correct behaviour and not what this test is about.
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

/// Refuse rather than clobber, and prove it by metadata rather than by
/// content.
///
/// This project has been bitten twice by exactly this. `shep style`'s writer
/// shipped a refusal that still rewrote the file (`d023465`): the bytes were
/// identical, so a content check passed, while the inode had changed and a
/// symlinked config would have been replaced by a regular file. Compare what
/// the filesystem says, not what the file says.
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

/// `--force` is the one destructive path, so it has to actually work.
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

/// `shep reload` reaches the reload verb, and the swap it starts really
/// finishes against real processes.
///
/// Two halves, and each covers something no other tier does. The envelope's
/// `command` is the only thing anywhere that pins which handler
/// `Commands::Reload` reaches — `main`'s dispatch arms have no unit coverage,
/// and an arm wired to `lifecycle::restart` would answer `"restart"` here
/// while otherwise behaving plausibly. The polled id is the other half: a
/// reload is the one verb whose success is a *new* id in the same instance
/// slot, so a restart in its place leaves the id where it was and a stop
/// leaves the sheep down.
///
/// The reply is asserted to carry the ORIGINAL id, which is the acceptance
/// contract — `shep reload` exits before the swap commits — and the poll is
/// what waits for the swap itself. A default `listen_timeout` of 3s plus a
/// drain fits inside [`FLOCK_DEADLINE`] with room to spare.
///
/// What a broken implementation this would catch: the dispatch misroute
/// above; a reload that answers and then never swaps (the poll expires and
/// the id assertion names what it wanted); a swap that leaves the drainee
/// registered, since the flock would still hold two entries and `data[0]`
/// would still be the original id.
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

/// `shep trigger` reaches the trigger verb and no other, against a real
/// daemon and a real sheep.
///
/// The envelope's `command` is the only thing anywhere that pins which
/// handler `Commands::Trigger` reaches — `main`'s dispatch arms have no unit
/// coverage (this file's own `reload_swaps_a_sheep_for_a_fresh_instance_
/// under_a_new_id` names the same gap for `Commands::Reload`) — so an arm
/// wired to some other verb's module would answer plausibly here (most of
/// them accept a selector, some even accept a second positional the shell
/// would happily supply) while never sending `Request::Trigger` at all.
///
/// The sheep here is started with no `channel`/`wait_ready`/
/// `shutdown_with_message`, so its own real reply is deterministic without
/// a companion process that speaks the shepherd channel: `no_channel`,
/// every time, on real wall-clock and a real daemon. That is also this
/// crate's own `output/rows.rs` `TriggeredRows` rendering exercised for
/// real, end to end, for the one outcome an operator hits by default —
/// building and driving a channel-speaking companion process for the other
/// three outcomes is real work of its own, left for later.
///
/// What a broken implementation this would catch: the dispatch misroute
/// above (verified by hand — routing `Commands::Trigger` to `query::ping`
/// instead of `trigger::trigger` leaves every unit test in the crate green
/// and only this case red); `TriggerArgs`'s `action` positional silently
/// dropped before it reaches `Request::Trigger`; and `no_channel`'s own
/// `DETAIL` text losing its `channel = true` callout.
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

/// `shep signal` reaches the signal verb and no other, against a real
/// daemon and a real sheep — the same dispatch-misroute gap `trigger`'s own
/// case names, for `Commands::Signal` instead.
///
/// `SIGWINCH` is the right signal for an e2e case: harmless to essentially
/// everything, so the assertion is about delivery reaching the sheep at all,
/// not about what the child did with it.
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

/// `shep stock` reaches the stock verb and no other, against a real daemon:
/// stocking up spawns the new instances, stocking back down drains the extras.
///
/// Both directions are polled through `shep flock` rather than trusted off
/// `stock`'s own exit — a stock-down accepts before the departing instances'
/// stop ladders finish (see `Commands::Stock`'s own doc), so the flock
/// settling to one row is the real assertion, and a stock-down that never
/// settles fails the test on `FLOCK_DEADLINE` rather than hanging it (IR-46).
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

/// `shep scale` is `stock`'s visible alias: it must still reach a real
/// daemon and produce the same primary-command name in its envelope as
/// `shep stock` does — the alias reaches the same verb, not a shadow of it.
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

/// `shep describe` renders a real sheep's lamb tree: the forked `sleep`
/// child appears in its own table, captioned with what the parent-pid walk
/// is and what it is not — the same caveat `output/mod.rs`'s own unit tests
/// pin against a hand-built `ProcessInfo` (Step 17.0/17.1), exercised here
/// over a real process tree end to end.
///
/// Polled rather than asserted on the first `describe`: the daemon walks for
/// lambs only inside its own `Describe` handler, against whatever the OS
/// process table happens to report at that instant, and the forked child's
/// appearance there is a real race against this test's own process —
/// bounded by `FLOCK_DEADLINE` rather than a fixed sleep (IR-46).
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

    // Polls for `sleep` specifically, not merely for a `Lambs of` section:
    // a walk can catch the forked child mid-exec, still reporting the
    // parent shell's own name — `sh` — rather than the program that pid is
    // about to become, so this loop rides out the `execve` as well as the
    // fork.
    //
    // Not a sampling tick, and the distinction is what makes the loop able
    // to ride it out at all: the 15-second memory poll is a different walk
    // entirely, and `MemorySampler::identify` builds a process table of its
    // own per call. Sharing the poll's retained table instead would make
    // this loop futile rather than slow — sysinfo never revises a name it
    // has already recorded for a pid, so every later iteration would read
    // the same `sh` back until the daemon restarted.
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

/// `shep save` writes the muster roll and `shep muster` reads it back — the
/// §13.4 flagship "import, muster, save, reboot" shape, minus the reboot: a
/// muster against the same still-live daemon that just saved exercises the
/// already-running idempotence rule (`snapshot::restorable`) for real, since
/// nothing here ever goes down in between.
///
/// Both dispatch arms carried no coverage beyond a clap-parsing pin
/// (`save_parses_to_its_own_command`/`muster_parses_to_its_own_command`,
/// `main.rs`) until this case: a dispatch arm in `run`'s `match` that calls
/// the wrong verb's function still compiles and still passes every
/// clap-parsing test, because nothing below the parse layer ever calls the
/// verb it parsed to. This case closes that gap for both verbs — confirmed
/// by rewiring `Commands::Save`/`Commands::Muster` to each other's function
/// in turn and watching this case redden each time.
///
/// What a broken implementation this would catch, beyond the dispatch
/// misroute above: a `save` that reports the wrong app count (`data.apps`
/// pins it at 1); a `muster` that reports `Started`'s shape instead of
/// `Mustered`'s, or that starts a *second* instance of `roundtrip` rather
/// than recognising the one already running (`flock.len()` pins exactly
/// one, `pid` pins it as the SAME process `start` reported, not a fresh
/// one) — the exact failure decision 3/4 exist to rule out, since a
/// duplicate or a restarted sheep here would silently double a real flock
/// on every reboot.
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

/// `shep import` reads a pm2 dump and writes a Flockfile shep can read
/// back, without ever touching a daemon.
///
/// The envelope's `command` is asserted for the same reason
/// `saving_the_roll_then_mustering_reports_the_same_flock` asserts it on
/// `save`/`muster`: `run`'s dispatch arms carry no unit coverage of their
/// own, and an arm reaching the wrong function (or handing it the wrong
/// args) would still exit 0 here without this. The written file is checked
/// against the REAL parser (`shep_core::config::Flockfile::parse`), not
/// merely "the process exited 0" — a Flockfile shep itself refuses to read
/// back is not an import. That no socket ever appears is the other half:
/// `import` takes no `Client` and starts nothing, and a dispatch arm that
/// somehow reached `connect_or_spawn_client` first would autostart a
/// daemon this verb never needs.
///
/// What a broken implementation this would catch: the dispatch misroute
/// above; a source resolution that ignores `--from` and reads the real
/// `~/.pm2/dump.pm2` instead; an `--out` silently ignored in favour of the
/// default `./Flockfile.toml`; and a renderer whose output this crate's own
/// `Flockfile::parse` refuses.
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
        !home.join("run/shep.sock").exists(),
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

/// Writes `$SHEP_HOME/shep.toml` directly, before any daemon has booted off
/// it. `shep enable`/`shep adopt` are this binary's only other writers of
/// that file, and neither has a flag for `[dog.metrics] bind` — a case that
/// needs a specific port (every case below does, to avoid colliding with a
/// real `9615` on the machine running this suite) has to put it there
/// itself, the same way [`write_flockfile`] writes a Flockfile directly
/// rather than driving `shep start` to produce one.
fn write_shep_toml(dir: &TempDir, body: &str) -> PathBuf {
    let path = dir.path().join("shep.toml");
    std::fs::write(&path, body).unwrap();
    path
}

/// The phase's own success criterion, at the only tier that can check it: a
/// real binary, a real shepherd, and a real dog PROCESS spawned by that
/// shepherd. Every tier below this one scripts the runner or fakes the
/// client, so none of them has ever exec'd `shep dog metrics` — an argv
/// branch that did not exist would fail at exec, which no unit test can
/// see.
///
/// Fails if the dog is not spawned (`wait_for_dog_pid` never sees a pid and
/// panics), if it cannot reach the socket from `$SHEP_HOME` — the one
/// variable a dog inherits (`dog/mod.rs`'s own module doc) — if it cannot
/// fetch its own `[dog.metrics]` section, or if it cannot bind and serve:
/// any of those leaves `poll_metrics` polling a refused connection for its
/// whole deadline and the body assertions below fail against an empty
/// string. Those four are the whole contract this case exists to prove.
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

    // Registered before the scrape, not after: a scrape that hangs or
    // panics on assertion must not leak the grandchild the daemon just
    // spawned. `wait_for_dog_pid` itself panics rather than returning
    // `None` on a pid that never arrives, for exactly this reason — see its
    // own doc.
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

/// Fails if `shep dogs` renders the sheep, or `shep flock` renders the dogs
/// into the sheep table. The two-table split (`FlockRows`/`DogRows`, and
/// `emit_flock`'s partition between them) has unit coverage of its own;
/// what this case adds is that the real verbs reach the real renderers over
/// a real daemon — the gap that would let a dispatch arm in `main::run`
/// point `Commands::Dogs` at the wrong function unnoticed workspace-wide,
/// the same class of bug `saving_the_roll_then_mustering_reports_the_same_
/// flock`'s own doc names for `save`/`muster`.
///
/// Table format, not JSON: `Format::Json`'s `flock` answer carries both
/// populations in one undivided array on purpose (`emit_flock`'s own doc),
/// so the "two populations, right way round" claim only exists in the
/// TABLE rendering this case has to read as text.
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

/// Fails if `shep barks` needs a shepherd. The history is on disk so it
/// outlives the daemon, and the case it exists for is an operator reading
/// it after a crash — this case never starts one at all, which is the
/// point: `shep barks` against a `$SHEP_HOME` with no `run/shep.sock` must
/// still succeed and render what `shep_core::barks::append` put on disk.
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
        !home.join("run/shep.sock").exists(),
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
        !home.join("run/shep.sock").exists(),
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

/// The whole store, through the real binary, with no shepherd anywhere. That
/// last part is the assertion that matters: `shep set` has to work on a
/// machine where nothing is running, because that is when provisioning
/// happens — the same claim [`barks_reads_the_history_with_no_shepherd_running`]
/// makes for `shep barks`.
///
/// Folds in the file-mode claim `shep_core::kv`'s own module doc makes
/// (`KV_FILE_MODE`, `0600`) rather than giving it a separate case: the file
/// this test's own first `set` creates is the one to check, and creating a
/// second store just to stat it would prove nothing this one doesn't.
///
/// What a broken implementation this would catch: a `set`/`get`/`unset`
/// dispatch that reached for `connect_client` instead of going straight to
/// `shep_core::kv` (any step here would then hang or exit
/// `DaemonUnreachable` instead of the codes asserted below); a store
/// created with the wrong mode; `unset` on a present key reporting anything
/// but success, or on an absent one reporting anything but `NotFound`.
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
        !home.join("run/shep.sock").exists(),
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

/// `shep --format json get` on the same shape of store `shep get` renders
/// as a table above: the envelope's `data` is an array of `{key, value}`
/// objects (never a JSON map — `KvRows`' own doc gives the reason), and
/// `schema_version` is `1` — pinned as a literal rather than imported from
/// `output::SCHEMA_VERSION`, since `shep-cli` is `[[bin]]`-only and this
/// file, an external test binary, has no lib target to import it from. Same
/// envelope shape every other verb in this binary produces. A key that is
/// not there and a key outside the grammar are checked here too, since
/// both surface as this envelope's error half.
///
/// What a broken implementation this would catch: `get`'s whole-store
/// listing degrading to a JSON object keyed by name (the one shape every
/// other consumer of this envelope would have to special-case); `unset`
/// reaching `NotFound` for a bad key instead of `Usage`, or the reverse.
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

/// Two real `shep set` PROCESSES — not two threads sharing one process's
/// open-file-description table — writing to the same store at once. This is
/// the CLI's own version of `shep_core::kv`'s own
/// `two_concurrent_writers_lose_nothing` unit test, and it exists because
/// that unit test's own two racers are `std::thread::spawn` inside ONE
/// `cli_e2e` test process: real, but not the claim `shep set` makes to two
/// operators running it from two separate shells. Proving that needs two
/// separate processes actually contending for `kv.json.lock`'s `flock(2)`,
/// which is what `Command::spawn` (via `shep`, wrapping `assert_cmd`) gives
/// here that two threads in this test binary could not.
///
/// The [`std::sync::Barrier`] is [`concurrent_cold_starts_produce_exactly_one_daemon`]'s
/// own synchronization, for the same reason: without it, OS scheduling could
/// let one writer finish its whole batch before the other starts a single
/// process, which would still pass the assertions below but would not
/// actually be racing anything.
///
/// What a broken implementation this would catch: a lock taken on the store
/// file itself rather than the sibling `.lock` file (the `rename` that
/// installs new content would then race the very lock guarding it); a fixed
/// temp-file name (one writer's `rename` consuming the other's staging file,
/// which reads here as a spurious `Io` error on one of the racers' outputs
/// rather than a clean loss of keys — either way `data.len()` comes up
/// short).
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

/// fails if `shep lookout` writes terminal escapes into a pipe. `assert_cmd`
/// captures stdout, so this exercises the not-a-tty refusal exactly as a
/// `shep lookout > dash.txt` would — and it is the case that keeps a redirected
/// dashboard from corrupting a file. Also proves the verb does not HANG
/// without a terminal, which is the regression that would cost CI a job rather
/// than a test (IR-46: `.timeout(CMD_TIMEOUT)` is on the chain).
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

/// fails if the `dash` alias stops reaching the same verb. Same refusal, same
/// code, through the other spelling.
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

/// fails if `--help` stops naming the gate. `--help` is where an operator
/// learns that the dashboard is read-only by default, and the flag's own text
/// is where they learn the gate is not a security boundary.
///
/// **Two assertions this test deliberately does not make.** It does not assert
/// `text.contains("dash")`: the verb's own about-text says "live dashboard"
/// and the flag's help says "the dashboard", so that substring is there
/// whether or not the alias is — delete `visible_alias` and it still passes.
/// The alias is pinned in `cli.rs`'s `alias_visibility_and_hiding_are_pinned`,
/// through `get_visible_aliases()`, which is the only assertion that can tell
/// the difference. And it asserts on `security boundary`, not on the whole
/// sentence: `wrap_help` is enabled on this crate's clap, so clap re-wraps
/// long help at the detected terminal width and a longer phrase can land
/// across a line break on one machine and not another.
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
//
// This is the only tier where whistle's stdout discipline can be observed at
// all — a real child process, real stdin/stdout, not a fake transport — so
// [`whistle_speaks_mcp_and_writes_nothing_else_to_stdout`]'s line-by-line
// assertion is this file's, not any lower tier's, to make.
// ---------------------------------------------------------------------------

/// Serializes `value` as compact JSON followed by `\n` — the newline-
/// delimited framing `transport-io`'s codec expects on both sides of the
/// pipe.
fn push_mcp_line(buf: &mut Vec<u8>, value: &serde_json::Value) {
    buf.extend_from_slice(value.to_string().as_bytes());
    buf.push(b'\n');
}

/// Stdin for one MCP session: the `initialize` handshake (id `1`), the
/// `notifications/initialized` that follows it, then each of `requests` in
/// order. `"2025-06-18"` is one of `ProtocolVersion::KNOWN_VERSIONS`
/// (rmcp `model.rs:181-187`) rather than the crate's current `LATEST`, so an
/// rmcp version bump does not redden this suite for no behavioural reason —
/// negotiation falls back to the server's own configured version either way.
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

/// A `tools/call` request with the given id, tool name, and (optional)
/// arguments object — omitted entirely rather than sent as `{}` when a tool
/// takes none, matching what a real client sends.
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

/// Parses every line of `stdout` as JSON-RPC, panicking naming the
/// offending line if any line fails to parse as JSON or lacks the
/// `"jsonrpc"` key.
///
/// This is this file's load-bearing assertion: a test that only searched
/// stdout for the reply it wanted would pass even if the verb also printed a
/// stray `println!`, a `--format json` error envelope, or a tracing record
/// onto the same wire. That includes a BARE `println!()`: `str::lines`
/// never yields a trailing empty entry for a well-formed final newline, so
/// any empty line left after splitting is a stray blank line the verb
/// wrote, not framing artefact, and a blank line is not JSON-RPC either —
/// this used to filter those out and let one through unnoticed.
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

/// The reply among `lines` whose `"id"` matches — a response, told apart
/// from a request/notification of the same shape by carrying `"result"` or
/// `"error"`.
fn find_reply(lines: &[serde_json::Value], id: i64) -> &serde_json::Value {
    lines
        .iter()
        .find(|line| {
            line.get("id") == Some(&serde_json::Value::from(id))
                && (line.get("result").is_some() || line.get("error").is_some())
        })
        .unwrap_or_else(|| panic!("no reply with id {id} in {lines:#?}"))
}

/// A `shep` invocation that reaches `$SHEP_HOME` through the `SHEP_HOME`
/// environment variable rather than `--home`. `GlobalArgs::home` carries
/// `env = "SHEP_HOME"` (`crates/shep-cli/src/cli.rs:29-31`), so clap folds
/// this the same way it folds the flag — [`shep`] and this must resolve the
/// same gate for [`the_shep_toml_gate_decides_the_tool_list_in_a_real_process`]
/// to mean anything.
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

/// fails if `shep whistle` stops speaking MCP, or starts writing anything
/// else to stdout. Drives the real binary: an `initialize` request and a
/// `tools/list` request, newline-delimited on stdin, replies read back from
/// stdout.
///
/// The stdout assertion is the load-bearing one and it is exact: EVERY line
/// stdout produces must parse as JSON with a `"jsonrpc"` key — see
/// [`assert_every_stdout_line_is_jsonrpc`]'s own doc for what that catches
/// that a search for the reply alone would not.
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

/// fails if the gate stops being read from `shep.toml`, end to end, in a
/// real process. THREE runs against two `$SHEP_HOME`s: no `[whistle]`
/// section (via the environment, five tools), `allow_control = true` (via
/// the environment, nine), and that same open directory again through
/// `--home` instead of the environment.
///
/// The five/nine split is checked by name, not only by count — a count
/// alone would pass if the gate accidentally registered a read tool twice.
///
/// Run 3 is not redundant: it pins that the launcher chooses which
/// `shep.toml` is read in argv exactly as it does in the environment (see
/// "Why there is no `--allow-control` flag" in the phase plan), which
/// reddens here if `resolve_paths` ever stops folding `--home` the same way
/// it folds `SHEP_HOME` (`crates/shep-cli/src/main.rs:112-123`).
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

/// fails if a `shep.toml` that exists but will not parse ever reaches
/// stdout, or opens the gate it failed to read. Every other case in this
/// file uses a `$SHEP_HOME` with either no `shep.toml` at all or one that
/// parses, so `whistle::whistle`'s one `output::emit_error` call — the
/// malformed-config stderr notice, the only thing whistle ever writes
/// outside the JSON-RPC wire — had never run under
/// [`assert_every_stdout_line_is_jsonrpc`]'s per-line check before this
/// test existed. That call sits right next to the stdout handle; a mistake
/// that pointed it at stdout instead of stderr would have gone uncaught.
///
/// Pins both halves: stdout stays pure JSON-RPC, and the gate reads SHUT —
/// a config that fails to parse must not fail OPEN — which is a behaviour
/// worth pinning on its own, not just a side effect of the stdout check.
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

/// fails if a gated-off control tool becomes callable. With the gate shut,
/// `tools/call` for `stop_sheep` must answer JSON-RPC error `-32602` with
/// `"tool not found"` — rmcp's own answer for a name its router does not
/// hold (`handler/server/router/tool.rs:570-571`).
///
/// This is the one case that proves ABSENCE rather than a refusal message: a
/// tool that existed and refused would answer a `result`, not an `error`.
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

/// fails if whistle stops starting when no shepherd is running. An MCP
/// server must answer `initialize` regardless — its transport is the
/// launcher's, not the shepherd's — and report the missing daemon per call
/// instead.
///
/// `$SHEP_HOME` here is a fresh tempdir with no daemon and no socket, so a
/// whistle that dialled at startup would fail to come up at all.
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

/// fails if `shep dogs --available` renders the raw parsed entry instead of
/// the sanitised one -- the failure this whole feature exists to prevent
/// (`dog_index`'s own module doc names it as the module's security
/// boundary). Rex's description carries a raw `\u{1b}[2J` screen-clear
/// escape; this asserts on the RAW stdout bytes, not a lossy string, so a
/// regression cannot hide behind `String::from_utf8_lossy`'s own
/// replacement character.
///
/// Also the "table lists a known index" case: both entries' NAME/PACKAGE/
/// CATEGORY reach the table, and the one sanitised entry is named in a
/// footer notice on stderr.
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

/// fails if the detail view's adopt line is built from `name` or `package`
/// instead of `adopt_as` -- `AvailableDog::adopt_as`'s own doc names this
/// as the one thing this feature must never get wrong: a dog cannot learn
/// the name it was adopted under, so the wrong name here ships a
/// copy-pasteable command that silently discards the dog's whole
/// `[dog.<name>]` config section.
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
        stdout.contains("$ cargo install --git https://github.com/TurtIeSocks/shep-log-rotate"),
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

/// fails if a filter matching nothing exits non-zero — decision (task-3
/// brief): an empty search result is an answer, not a failure.
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

/// fails if `--available` reaches `connect_client` at all — the property
/// its guard arm in `main`'s dispatch (`lib.rs`) exists to guarantee.
/// `$SHEP_HOME` here is a fresh tempdir where no daemon has ever run,
/// mirroring `whistle_starts_with_no_shepherd_and_reports_it_per_call`'s
/// own setup; beyond the exit code, this checks that neither a socket nor
/// a pidfile exists afterwards, so a regression that autostarts a
/// shepherd as a side effect would be caught even if it still answered
/// successfully.
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
        !home.path().join("run/shep.sock").exists(),
        "--available must never bring up a shepherd"
    );
    assert!(
        !home.path().join("pids/shepd.pid").exists(),
        "--available must never bring up a shepherd"
    );
}

/// fails if a non-2xx from the index host panics, hangs, or reports an
/// error that does not name the URL — `IndexError` deliberately carries
/// the URL on none of its variants but its own `InsecureUrl`, so the
/// caller (`available_dogs`) is what has to tell the operator which URL
/// failed.
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

/// fails if a connection that closes mid-body panics or hangs instead of
/// reporting a clear, URL-naming error — the other server misbehaviour the
/// task-3 brief calls out by name, alongside the plain 500 above.
#[test]
fn available_dogs_reports_a_truncated_body_naming_the_url() {
    let home = TempDir::new().unwrap();
    // Declares 100 bytes of body, sends 2, then the server thread closes —
    // `fetch::get`'s own `Truncated` refusal (`fetch.rs`'s own doc).
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

/// fails if `shep serve` does not register a sheep, or registers one that
/// cannot actually serve. The assertion is an HTTP GET against the port, not
/// a `shep flock` row — a row says the process is up, and up is not serving.
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

/// fails if a missing docroot registers a crash-looping sheep instead of
/// failing immediately.
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

    // No daemon was ever spawned to register anything against — the
    // strongest available proof that the refusal happened before any
    // `Request::Start`, not that a registered sheep crash-looped after one.
    assert!(
        daemon_pid(dir.path()).is_none(),
        "a refused root must not even bring a shepherd up"
    );
}

/// fails if the worker ignores SIGTERM. Step 6.2 copies the metrics dog's
/// signal handling and states the failure mode — a worker that only handles
/// SIGINT rides the whole kill ladder to SIGKILL on every `shep stop`, which
/// is slow and looks like a hang.
///
/// See [`SERVE_STOP_DEADLINE`]'s own doc for why this is a deterministic
/// assertion on `shep stop`'s own wall-clock rather than a flaky one.
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

/// Layout shared by the two `--follow-symlinks` cases below:
/// `<root>/releases/2026-08-15/index.html` and
/// `<root>/current -> releases/2026-08-15` — the exact deploy shape Rin's
/// ruling names.
fn write_deploy_layout(root: &Path) {
    let release = root.join("releases/2026-08-15");
    std::fs::create_dir_all(&release).unwrap();
    std::fs::write(release.join("index.html"), "the deploy layout").unwrap();
    std::os::unix::fs::symlink(&release, root.join("current")).unwrap();
}

/// fails if the per-refusal stderr line (decision 5, Rin's ruling) never
/// reaches the sheep's own bleats. This is the one claim in the ruling that
/// Task 3's and Task 6's in-process tests cannot make: they run inside the
/// test binary's own process, sharing its stderr with every other test in
/// the suite, so asserting real output there would mean hijacking a
/// process-global stream under `cargo test`'s default thread-per-test
/// concurrency — flaky by construction. A registered sheep is a real child
/// process with its own captured stderr, which is exactly what `shep
/// bleats` already reads; that is the one place this claim can be checked
/// honestly.
///
/// Registered WITHOUT `--follow-symlinks`.
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

/// fails if `--follow-symlinks` does not actually serve the deploy layout
/// end to end, through registration and a restart, and fails if setting it
/// stops being loud at startup. Two assertions in one test because they are
/// one scenario: the flag that makes the deploy layout work is the same
/// flag `follow_symlinks_notice` announces.
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

/// fails if `shep runtime` does not exit on its own when the flock empties,
/// or exits with the wrong code for the reason it emptied.
///
/// Two runs of the same shape, sharing one test so the harness's own
/// `Command::cargo_bin` lookup is paid once: one app that exits 0 with
/// `autorestart = false` (exit 0, a clean batch job), and one that exits 1
/// with `max_restarts = 1` (exit 11) — `max_restarts = 1` errors on the
/// FIRST unstable exit (`shep-daemon`'s `entry.rs::exhausted`: N = 1 means
/// N-1 = 0 restarts performed), so this needs no wait through a restart
/// delay. The second case is decision 13's whole contract.
///
/// Each takes at least 6 seconds (`commands::empty::STRIKES` × `INTERVAL` =
/// 3 × 2s) — the debounce is not shortened to make this fast, per Step
/// 9.5's own plan text — so this one test costs a bit over 12 seconds of
/// the suite's wall clock.
///
/// No [`DaemonGuard`] here, unlike almost every other case in this file:
/// `shep runtime` never leaves a daemon behind on either path (that is the
/// whole point of the verb — no daemonizing, no re-exec, nothing left
/// running once the flock empties), so there is nothing for a guard to
/// adopt. A hang instead of a clean exit is caught by `shep()`'s own
/// `CMD_TIMEOUT`.
#[test]
fn runtime_exits_when_the_flock_empties_with_a_code_that_says_why() {
    // Clean emptying: one app exits 0 and is told not to restart.
    let clean_dir = tempfile::tempdir().unwrap();
    let clean_script = write_script(&clean_dir, "clean.sh", "#!/bin/sh\nexit 0\n");
    let clean_flockfile = write_flockfile(
        &clean_dir,
        &format!(
            "[[app]]\nname = \"batch\"\nscript = \"{}\"\nautorestart = false\n",
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
            "[[app]]\nname = \"batch\"\nscript = \"{}\"\nmax_restarts = 1\n",
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

// --- `shep dev` -------------------------------------------------------

/// A `shep dev` invocation with `$SHEP_DEV_HOME` set to `dev_home`, timeout
/// already attached. Never `--home` — decision 15: `dev` ignores it, so a
/// helper that carried one would misrepresent what a real invocation looks
/// like.
fn shep_dev(dev_home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("shep").unwrap();
    cmd.env("SHEP_DEV_HOME", dev_home)
        .arg("dev")
        .timeout(CMD_TIMEOUT);
    cmd
}

/// Copies `source` to nowhere, on a background thread. `shep dev` streams
/// the flock's bleats to its own stdout for as long as it runs; if nothing
/// drains that pipe it eventually fills and blocks the child, wedging
/// [`spawn_shep_dev`]'s caller. Mirrors `tests/init.rs`'s own helper of the
/// same name and shape — a shared one is not worth a `tests/support` module
/// for the two files that need it.
fn discard_in_background<R: Read + Send + 'static>(mut source: R) {
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut source, &mut std::io::sink());
    });
}

/// Spawns `shep dev <flockfile>` with `$SHEP_DEV_HOME` set to `dev_home`,
/// stdout and stderr both piped and immediately drained in the background.
/// Unlike [`shep_dev`]'s `.output()` shape, this leaves the process alive so
/// a caller can signal it — the point of
/// [`dev_tidies_up_when_it_is_signalled_rather_than_when_the_flock_empties`],
/// which needs `shep dev` still running when `SIGTERM` arrives.
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

/// Polls `shep --home <dev_home> --format json flock` until the one app's
/// row reports `online`, and returns that row. Tolerates the early window
/// where `shep dev` has not bound its socket yet — unlike
/// [`poll_flock_data`], which asserts success on every attempt and is only
/// safe once a socket is already known to exist.
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

/// Polls `child.try_wait()` until it exits, or `timeout` elapses — a named
/// panic instead of relying on `CMD_TIMEOUT`'s own kill inside `.output()`,
/// which does not apply here since this file's `spawn_shep_dev` never calls
/// `.output()`. Mirrors `tests/init.rs`'s own `wait_bounded`.
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

/// fails if `shep dev` leaves a shepherd or a flock behind. Runs a script
/// that exits immediately with `autorestart = false`, so the auto-exit
/// fires (the same debounce `runtime_exits_when_the_flock_empties_with_a_
/// code_that_says_why` drives, `commands::empty::STRIKES` × `INTERVAL` = 3
/// × 2s), then asserts the dev home has no live socket and that a `shep`
/// pointed at that same home afterward finds no shepherd left to answer
/// `flock` — decision 15's `tidy_up: true`, the one setting this case
/// actually exercises (Step 11.4's own mutation: `tidy_up: false` reddens
/// this on the second assertion, not the first).
///
/// `$SHEP_DEV_HOME` points at its own tempdir, never the harness's real
/// `~/.shep-dev` — decision 15's second reason for the variable existing.
#[test]
fn dev_tidies_up_after_itself() {
    let dir = tempfile::tempdir().unwrap();
    let dev_home = tempfile::tempdir().unwrap();
    let script = write_script(&dir, "batch.sh", "#!/bin/sh\nexit 0\n");
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"batch\"\nscript = \"{}\"\nautorestart = false\n",
            script.display(),
        ),
    );

    let output = shep_dev(dev_home.path()).arg(&flockfile).output().unwrap();
    assert_success(&output);

    let socket = dev_home.path().join("run/shep.sock");
    assert!(!socket.exists(), "dev must not leave a live socket behind");

    let flock_output = shep(dev_home.path()).arg("flock").output().unwrap();
    assert!(
        !flock_output.status.success(),
        "no shepherd should remain at the dev home to answer `flock`: {flock_output:?}"
    );
}

/// fails if Ctrl-C out of `shep dev` leaves a shepherd or a flock behind —
/// on disk as well as in the process table.
///
/// The auto-exit path above never sends a signal, and the signal path is
/// the one people actually use — "a dev mode that leaks a supervisor is a
/// dev mode people stop trusting" is `commands::dev`'s own claim and
/// nothing else in this file checks it. Runs a long-lived script so nothing
/// exits on its own, waits for the flock to reach `online` (proof the
/// shepherd is up and the sheep is running, not merely that the process
/// exists) and records the sheep's own pid, sends `SIGTERM` to the `shep
/// dev` process itself, and asserts the same two things the auto-exit case
/// does — no live socket, no shepherd left running — plus two only this
/// case can make: the held sheep did not outlive its supervisor, and
/// `flock.json` does not still list it as running (Phase 15 review,
/// Important 2 — a signal reaches `commands::foreground::run`'s own
/// `RunningDaemon::run` teardown directly, never through the `Stop`/
/// `Delete` pair `tidy_up` sends over the wire, so only
/// `BootOptions::delete_flock_on_shutdown` closes this half of the gap).
#[test]
fn dev_tidies_up_when_it_is_signalled_rather_than_when_the_flock_empties() {
    let dir = tempfile::tempdir().unwrap();
    let dev_home = tempfile::tempdir().unwrap();
    let script = write_script(&dir, "held.sh", "#!/bin/sh\nsleep 60\n");
    let flockfile = write_flockfile(
        &dir,
        &format!(
            "[[app]]\nname = \"held\"\nscript = \"{}\"\n",
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

    let socket = dev_home.path().join("run/shep.sock");
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

/// fails if `shep-dev` or `shep-runtime` is not built, is not installed
/// under that name, or does not reach its own verb. `--help` rather than a
/// real run, so the test starts no shepherd and writes to no home.
///
/// **The assertion is the usage line, not the verb's name.** Once Tasks 9
/// and 11 add the verbs, the ROOT `shep --help` lists `dev` and `runtime`
/// among its subcommands — so `text.contains("dev")` passes even if
/// `alias_argv` is deleted entirely and the binary prints root help.
/// `Usage: shep dev` is printed only by that subcommand's own help. This is
/// the plan's sixth dead-check shape, in the one test that covers the alias
/// binaries at all.
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

// --- Whole-branch review item 4 -------------------------------------------

/// Shared by the case below: no ANSI escape byte, and none of the
/// box-drawing glyphs `render_boxed` (`shep-cli/src/output/table.rs`) draws
/// -- the hard rule's own two failure shapes, named once so a second verb's
/// assertion cannot silently drift from the first's.
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

/// The spec's own claim (§5): "The existing e2e suite is the pipe test...
/// If a border or an escape reaches piped stdout, it fails. No new test
/// needed." False as this file stood: every table-shaped assertion above is
/// `--format json`, which `must_render_bare` forces to `Bare` on its own
/// separate axis regardless of terminal-ness, and the only table-mode
/// stdout assertions anywhere in this file are `.contains(...)` checks on
/// `bleats` log lines. A box border reaching piped `shep flock` at the
/// default style would have left all of this file green.
///
/// `assert_cmd`'s `.output()` captures stdout through an OS pipe, never a
/// pty, so `std::io::stdout().is_terminal()` is `false` for every
/// invocation in this whole suite -- exactly `must_render_bare`'s own
/// trigger (`lib.rs`), exercised here with no `--format json` and no
/// `--style` flag at all: the plain `shep flock | less` / `shep flock >
/// file` an operator actually types. This is the safety net for the single
/// most important rule on the branch, and until this case it was guarded
/// only by a unit test of the predicate (`must_render_bare_is_true...`,
/// `lib.rs`) plus renderer tests handed `Presentation::BARE` by hand
/// (`table.rs`'s own snapshots) -- never the real wiring between clap
/// parsing a piped invocation and the byte this binary actually writes to
/// the pipe `assert_cmd` opened.
///
/// Two verbs, not one: `flock` and `describe` go through `emit_flock`/
/// `emit_described` respectively, both bespoke wrappers around `table_of`
/// rather than a plain `emit` call (`output/mod.rs`'s own module doc), so a
/// regression scoped to just one of the two would still pass a case that
/// only ever tried the other.
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

/// Issue 1's first repro, verbatim: `cargo install shep-log-rotate` puts
/// the binary on `$PATH` under its own name, and `shep adopt` used to be
/// unable to find it there at all.
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
        written.contains(&binary.display().to_string()),
        "the $PATH hit must be the recorded binary: {written}"
    );
}

/// Issue 1's second repro, verbatim: a literal `~/` path, which worked in
/// a Flockfile (2026-08-19) but not at `shep adopt` until now.
#[test]
fn shep_adopt_expands_a_leading_tilde_path() {
    let shep_home = TempDir::new().unwrap();
    let fake_user_home = TempDir::new().unwrap();
    let bin_dir = fake_user_home.path().join(".cargo/bin");
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
        written.contains(&binary.display().to_string()),
        "the ~/-expanded binary must be the one recorded: {written}"
    );
}
/// Writes a script that records its own argv and `$SHEP_HOME` into `marker`
/// (inside `dir`), prints a distinctive stdout line, and exits `code` --
/// the fixture [`an_adopted_dog_runs_directly_with_its_own_argv_and_shep_home`]
/// and [`a_built_in_verb_always_wins_over_a_same_named_adopted_dog`] both
/// build on.
fn write_marker_script(dir: &TempDir, marker: &Path, code: u8) -> PathBuf {
    write_script(
        dir,
        "dog.sh",
        &format!(
            "#!/bin/sh\necho \"argv:$*\" > \"{marker}\"\necho \"home:$SHEP_HOME\" >> \"{marker}\"\necho from-the-dog\nexit {code}\n",
            marker = marker.display(),
        ),
    )
}

/// `shep <dogname> [args...]` (issue 3): once a dog is adopted, invoking
/// its name directly runs it -- with the operator's own argv passed
/// through untouched and `$SHEP_HOME` set, the "operator-invoked" contract
/// that is deliberately distinct from the supervised one (no argv, that
/// same one env entry) a shepherd-started dog gets.
///
/// The dispatch call itself carries no `--home` flag at all -- `$SHEP_HOME`
/// is set through the environment instead, exercising `home_before`'s
/// fallback to the real environment alongside its `--home`-flag form,
/// which the lib-tier `home_before_*` tests already cover directly.
///
/// Mutation check: reverting `lib.rs`'s `dispatch_adopted_dog` to always
/// return `None` reddens this immediately -- clap's own "unrecognized
/// subcommand" error and exit code 2 instead of the dog's own exit code 7.
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

/// Built-in verbs always win, structurally (issue 3): a `[daemon]
/// adopted_dogs` entry named `stop` -- written directly, bypassing `shep
/// adopt`'s own refusal of the name, the way a hand-edited `shep.toml`
/// could -- must never shadow the real `shep stop`. `dispatch_adopted_dog`
/// only ever runs once clap has already failed to match a token against a
/// real subcommand, so `stop` never reaches it at all.
///
/// `shep stop all` against a `$SHEP_HOME` with no shepherd running exits
/// `DaemonUnreachable` (5) -- `commands::lifecycle::stop` goes through
/// `connect_client`, which does not autostart -- so that exit code, and the
/// marker file never appearing, are both proof the built-in ran (or at
/// least was the one dispatch attempted) rather than the dog's script.
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

/// An unrecognized verb with no matching adopted dog stays an ordinary
/// unknown-verb error, suggestions included -- `dispatch_adopted_dog`
/// finding nothing must fall all the way through to clap's own rendering,
/// not a silent or different failure. No dog is adopted at all here, and
/// `$SHEP_HOME` does not even exist yet.
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
