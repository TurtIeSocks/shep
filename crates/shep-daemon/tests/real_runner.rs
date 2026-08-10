//! Behavioral tests for [`shep_daemon::tokio_runner::TokioRunner`] against
//! real `/bin/sh` child processes.
//!
// real time: integration tier exercises the actual OS; IR-38 deviation
// deliberate: behavioral OS tests need a separate binary so unit tests stay
// paused-clock-pure. Every `#[tokio::test]` here runs on the real (unpaused)
// clock — there is no `start_paused = true`.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use shep_daemon::channel::ChildMessage;
use shep_daemon::privilege::Credentials;
use shep_daemon::runner::{ProcessRunner, RunningProcess, SpawnSpec, StopSignal};
use shep_daemon::tokio_runner::TokioRunner;

/// Builds a `/bin/sh -c <script>` spec writing logs into a fresh tempdir.
///
/// WHY a helper: every test needs the same boilerplate (shell program, fresh
/// log paths) but its own script and channel flag (IR-34: unique scenario).
fn sh_spec(script: &str, channel: bool, out_file: PathBuf, err_file: PathBuf) -> SpawnSpec {
    SpawnSpec {
        name: "real-runner-test".to_string(),
        program: "/bin/sh".to_string(),
        args: vec!["-c".to_string(), script.to_string()],
        cwd: None,
        env: BTreeMap::new(),
        out_file,
        err_file,
        channel,
        credentials: None,
    }
}

/// Builds a `program args...` spec writing logs under `dir` — the general
/// form `sh_spec` doesn't cover (an arbitrary program, not always `/bin/sh
/// -c <one script string>`). Used by the uid/gid drop proof below, which
/// needs to run `/bin/sh -c "id -u"` and separately by nothing else today,
/// but is named/shaped for reuse (Task 8 brief: "this file's existing
/// helper" — added here since it didn't exist before this task).
fn spec_for(dir: &tempfile::TempDir, program: &str, args: &[&str]) -> SpawnSpec {
    SpawnSpec {
        name: "real-runner-test".to_string(),
        program: program.to_string(),
        args: args.iter().map(|s| (*s).to_string()).collect(),
        cwd: None,
        env: BTreeMap::new(),
        out_file: dir.path().join("out.log"),
        err_file: dir.path().join("err.log"),
        channel: false,
        credentials: None,
    }
}

/// How long a log line gets to travel from the pump's `write_all` to the
/// file. A write that is going to land lands in microseconds; this is slack
/// for a loaded runner, not an expected duration.
const LOG_WRITE_DEADLINE: Duration = Duration::from_secs(5);

/// Waits for `path` to hold exactly `expected`, failing at
/// [`LOG_WRITE_DEADLINE`].
///
/// Polls rather than sleeping a fixed guess, the same shape as
/// [`assert_reaped`] below and for the same reason: the wait is bounded by
/// what must eventually be true, not by a number someone picked.
async fn await_file_contents(path: &Path, expected: &str) {
    let settled = tokio::time::timeout(LOG_WRITE_DEADLINE, async {
        while fs::read_to_string(path).unwrap_or_default() != expected {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        settled.is_ok(),
        "{}: expected {expected:?}, found {:?}",
        path.display(),
        fs::read_to_string(path)
    );
}

#[tokio::test]
async fn exit_code_and_logs_are_captured() {
    let dir = tempfile::tempdir().unwrap();
    let out_file = dir.path().join("out.log");
    let err_file = dir.path().join("err.log");
    let runner = TokioRunner::new();
    let spec = sh_spec(
        "echo out-line; echo err-line 1>&2; exit 7",
        false,
        out_file.clone(),
        err_file.clone(),
    );

    let (mut proc, mut io) = runner.spawn(&spec).unwrap();

    let mut saw_out = false;
    let mut saw_err = false;
    for _ in 0..2 {
        let line = io
            .logs
            .recv()
            .await
            .expect("logs channel closed before both lines arrived");
        match (line.err, line.line.as_str()) {
            (false, "out-line") => saw_out = true,
            (true, "err-line") => saw_err = true,
            other => panic!("unexpected log line: {other:?}"),
        }
    }
    assert!(saw_out, "missing stdout line");
    assert!(saw_err, "missing stderr line");

    let outcome = proc.wait().await;
    assert_eq!(outcome.code, Some(7));
    assert_eq!(outcome.signal, None);

    // Observing a line on `logs` means the pump ISSUED that line's file
    // write before forwarding it, not that the write landed: `tokio::fs`
    // copies into its own buffer and dispatches the real `write(2)` to the
    // blocking pool, so `write_all().await` returning means queued. It is
    // reliable in practice — at most one write is ever in flight, and the
    // next one awaits the previous operation — but that is an implementation
    // detail rather than a contract, so these read the file back on a
    // bounded poll instead of resting on it.
    await_file_contents(&out_file, "out-line\n").await;
    await_file_contents(&err_file, "err-line\n").await;
}

/// A shell fragment that blocks until `marker` exists, polling once a
/// second. `sleep`'s only portable argument is a whole number of seconds
/// (POSIX), so a finer poll would be a bet on a particular `sleep`.
///
/// Used to put a real child's output either side of a reopen without
/// guessing at timing: the test writes the marker exactly when it wants the
/// next line, so "after" can only have been written after the swap.
fn wait_for_marker(marker: &Path) -> String {
    format!("while [ ! -f {} ]; do sleep 1; done", marker.display())
}

/// Fails if a reopen leaves the pump writing into the renamed inode — which
/// is `create`-mode rotation (rename, then ask) silently producing an empty
/// live log forever, with `bleats --no-follow` printing nothing and exiting
/// 0.
///
/// Both halves are the assertion. A test that only checked the recreated
/// path grows would also pass against a pump that opened a SECOND handle
/// and kept the first: the new lines would land in both files. Checking
/// that the archive stopped growing is what pins the old handle as closed.
///
/// The duplex-stream cases in `tokio_runner.rs` cover the same swap without
/// a child. This one is the real article — a real fork, real pipes, a real
/// inode under the rename — which is the only tier where the child's own
/// view of its stdout could be disturbed by the swap, and the reason
/// nothing child-side is asked for: it holds a pipe, never the file.
#[tokio::test]
async fn a_reopen_moves_a_real_childs_output_onto_the_recreated_path() {
    let dir = tempfile::tempdir().unwrap();
    let out_file = dir.path().join("out.log");
    let err_file = dir.path().join("err.log");
    let marker = dir.path().join("go");
    let runner = TokioRunner::new();
    let spec = sh_spec(
        &format!("echo before; {}; echo after", wait_for_marker(&marker)),
        false,
        out_file.clone(),
        err_file.clone(),
    );

    let (mut proc, mut io) = runner.spawn(&spec).unwrap();
    // The child blocks on the marker, so any assertion that fails before
    // it is written leaves a real process behind for the rest of the run.
    let _reaper = Reaper(vec![i32::try_from(proc.pid()).unwrap()]);

    let line = tokio::time::timeout(LOG_WRITE_DEADLINE, io.logs.recv())
        .await
        .expect("the child's first line must arrive")
        .expect("logs closed before the first line");
    assert_eq!(line.line, "before");
    await_file_contents(&out_file, "before\n").await;

    // The rotator's half: the pump's handle now names an inode the live
    // path no longer resolves to.
    let archive = dir.path().join("out.log.1");
    fs::rename(&out_file, &archive).unwrap();
    assert!(!out_file.exists(), "sanity: the rename really moved it");

    let (done, ack) = tokio::sync::oneshot::channel();
    io.log_ctl
        .send(shep_daemon::runner::LogCtl::Reopen { done })
        .await
        .expect("a running sheep's pump must still be reachable");
    let outcome = tokio::time::timeout(LOG_WRITE_DEADLINE, ack)
        .await
        .expect("the reopen must be acknowledged")
        .expect("the pump must answer rather than drop the acknowledgement");
    assert_eq!(
        outcome,
        Ok(()),
        "the live path is there to be opened: the rename moved the inode, not the directory"
    );

    // No polling: the acknowledgement is a real barrier, since the reopen
    // flushes the old handle before dropping it.
    assert_eq!(fs::read_to_string(&out_file).unwrap(), "");
    assert_eq!(fs::read_to_string(&archive).unwrap(), "before\n");

    fs::write(&marker, "").unwrap();
    let line = tokio::time::timeout(LOG_WRITE_DEADLINE, io.logs.recv())
        .await
        .expect("the child's second line must arrive")
        .expect("logs closed before the second line");
    assert_eq!(line.line, "after");

    await_file_contents(&out_file, "after\n").await;
    assert_eq!(
        fs::read_to_string(&archive).unwrap(),
        "before\n",
        "the renamed file must stop growing the moment the handle is swapped"
    );

    let outcome = tokio::time::timeout(REAP_DEADLINE, proc.wait())
        .await
        .expect("the child exits once it has printed its second line");
    assert_eq!(outcome.code, Some(0));
}

#[tokio::test]
async fn signal_ignored_then_kill_tree_reaps() {
    let dir = tempfile::tempdir().unwrap();
    let runner = TokioRunner::new();
    let spec = sh_spec(
        r#"trap "" TERM; while true; do sleep 1; done"#,
        false,
        dir.path().join("out.log"),
        dir.path().join("err.log"),
    );

    let (mut proc, _io) = runner.spawn(&spec).unwrap();

    // Real sleep: give the shell time to actually execute `trap "" TERM`
    // before we signal it — without this grace period the signal can win a
    // race against the shell's own startup and kill it via the default
    // (untrapped) disposition.
    tokio::time::sleep(Duration::from_millis(100)).await;
    proc.signal(StopSignal::Term).unwrap();

    // Real sleep: give the (TERM-ignoring) child a real window to have exited
    // if our signal wrongly killed it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let still_running = tokio::time::timeout(Duration::from_millis(1), proc.wait()).await;
    assert!(
        still_running.is_err(),
        "process should still be running after an ignored SIGTERM"
    );

    proc.kill_tree().unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), proc.wait())
        .await
        .expect("kill_tree should reap promptly");
    assert_eq!(outcome.signal, Some(9));
}

/// How long the forked grandchild in
/// [`a_graceful_stop_reaches_a_forked_grandchild`] sleeps: comfortably longer
/// than [`REAP_DEADLINE`], so a passing run proves the stop signal reached it
/// rather than that it finished on its own; short enough that a run panicking
/// before [`Reaper`] fires leaves nothing lingering for a whole CI job.
const ORPHAN_SLEEP_SECS: u32 = 30;

/// How long [`assert_reaped`] waits for a pid to leave the process table. A
/// signal that lands takes milliseconds; this is slack for a loaded runner,
/// not an expected duration.
const REAP_DEADLINE: Duration = Duration::from_secs(5);

/// Last-resort net for a test that PANICS with real processes still alive, so
/// a failing assertion never leaks a 30-second `sleep` into the rest of the
/// run.
///
/// Fires ONLY while panicking: on the success path the test has already
/// proven these pids are gone, and signalling a pid the OS may since have
/// recycled is a hazard rather than a safety net.
///
/// SIGKILLs the whole process GROUP (`-pid`, not `pid`), modelled on
/// `daemon_e2e.rs`'s `Fixture::drop`, which has done so all along and states
/// the reason: `TokioRunner` spawns every child as its own group leader, so
/// the group signal also reaches a forked `sleep` grandchild that a
/// leader-only signal misses. A leader-only `Reaper` agreed with its sibling
/// on the happy path of
/// [`a_graceful_stop_reaches_a_forked_grandchild`] only because that test
/// pushes its grandchild's pid explicitly — and a panic anywhere between the
/// spawn and that push (the group-leader assertion and the five-second wait
/// for the wrapper's `echo $!` both live in that window) left an untracked
/// `sleep` behind with nothing tracking it. That same test asserts the
/// leader property this signal depends on, so `-pid` is safe exactly where it
/// is needed.
struct Reaper(Vec<i32>);

impl Drop for Reaper {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        for &pid in &self.0 {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(-pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

/// Polls `kill(pid, None)` for `ESRCH` instead of sleeping a fixed guess —
/// the same technique, for the same reason, as `daemon_e2e.rs`'s own
/// `assert_reaped` (integration binaries are separate crates, so the helper
/// is duplicated rather than shared). `kill(pid, None)` still returns `Ok`
/// for a zombie, so only a transition all the way to `ESRCH` proves the
/// process is really gone rather than exited-but-unreaped.
async fn assert_reaped(pid: i32, what: &str) {
    let reaped = tokio::time::timeout(REAP_DEADLINE, async {
        loop {
            match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
                Err(nix::errno::Errno::ESRCH) => break,
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await;
    assert!(reaped.is_ok(), "{what} (pid {pid}) is still alive");
}

/// The orphan regression: a graceful stop must reach a sheep's forked lambs,
/// not just the sheep.
///
/// The wrapper is the shape that used to leak — it forks a long-lived child
/// and does NOT `exec` it, so the child is a separate process sitting in the
/// sheep's own process group. `signal` targeted the leader alone, the wrapper
/// died promptly out of its `wait`, `proc.wait()` returned `Ok` so the ladder
/// never escalated to `kill_tree`, and the `sleep` ran on — reparented,
/// untracked, invisible to `shep list` and to the next `shep kill` alike.
///
/// Only the grandchild tells the two behaviors apart: the wrapper exits on
/// `SIGTERM` either way. Nothing here sleeps for a fixed guess — the fork is
/// awaited via the pid the wrapper prints AFTER forking, and the grandchild's
/// death via [`assert_reaped`]'s `ESRCH` poll.
#[tokio::test]
async fn a_graceful_stop_reaches_a_forked_grandchild() {
    let dir = tempfile::tempdir().unwrap();
    let runner = TokioRunner::new();
    let spec = sh_spec(
        &format!("sleep {ORPHAN_SLEEP_SECS} & echo $!; wait"),
        false,
        dir.path().join("out.log"),
        dir.path().join("err.log"),
    );

    let (mut proc, mut io) = runner.spawn(&spec).unwrap();
    let leader = i32::try_from(proc.pid()).unwrap();
    let mut reaper = Reaper(vec![leader]);

    // Pins the property both stop rungs' negative-pid signal depends on
    // (`tokio_runner.rs`'s `signal_group`): a runner that dropped
    // `process_group(0)` would leave `-pid` naming no group at all, and every
    // assertion below would fail for a reason this makes explicit.
    assert_eq!(
        nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(leader)))
            .unwrap()
            .as_raw(),
        leader,
        "a spawned sheep must lead its own process group"
    );

    // The wrapper prints `$!` only after the fork has happened, so receiving
    // this line is proof the grandchild exists — no grace-period sleep needed
    // to close the race the other tests in this file have to.
    let line = tokio::time::timeout(Duration::from_secs(5), io.logs.recv())
        .await
        .expect("the wrapper must report its forked child's pid")
        .expect("logs channel closed before the pid arrived");
    let grandchild: i32 = line.line.trim().parse().expect("`echo $!` prints a pid");
    reaper.0.push(grandchild);
    assert_ne!(grandchild, leader, "sanity: `&` really forked");

    proc.signal(StopSignal::Term).unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), proc.wait())
        .await
        .expect("the wrapper must exit on SIGTERM");
    assert_eq!(
        outcome.signal,
        Some(StopSignal::Term.as_raw()),
        "the wrapper itself dies of the same signal either way"
    );

    // The assertion the whole test exists for.
    assert_reaped(grandchild, "the wrapper's forked child").await;
}

#[tokio::test]
async fn shepherd_channel_delivers_ready() {
    let dir = tempfile::tempdir().unwrap();
    let runner = TokioRunner::new();
    let spec = sh_spec(
        r#"printf '{"kind":"ready"}\n' >&3; sleep 5"#,
        true,
        dir.path().join("out.log"),
        dir.path().join("err.log"),
    );

    let (mut proc, mut io) = runner.spawn(&spec).unwrap();

    let msg = tokio::time::timeout(Duration::from_secs(5), io.from_child.recv())
        .await
        .expect("shepherd-channel Ready should arrive promptly")
        .expect("from_child closed before Ready arrived");
    assert_eq!(msg, ChildMessage::Ready);

    // Real sleep: `printf ... >&3` is a shell builtin that returns (and lets
    // us observe Ready) before the shell has necessarily forked its `sleep
    // 5` child. Without this grace period, kill_tree's group-wide SIGKILL
    // can win that race and leave `sleep 5` behind as an orphan (its fork
    // simply hadn't happened yet when the kernel delivered the signal).
    tokio::time::sleep(Duration::from_millis(100)).await;
    proc.kill_tree().unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), proc.wait())
        .await
        .expect("kill_tree should reap promptly");
    assert_eq!(outcome.signal, Some(9));
}

/// Proves `TokioRunner::spawn`'s `command.uid(creds.uid)` /
/// `command.gid(gid)` lines (the ones a whole-branch review found a
/// reviewer could delete with the full suite staying green — the only
/// other coverage was the root-only, `#[ignore]`d test just below, plus
/// `fake.rs:401` which hardcodes `credentials: None` and so never reaches
/// this code at all) are ACTUALLY CALLED, without requiring root.
///
/// The obvious version of this test — spawn with your OWN uid/gid and
/// assert the child reports them back — does NOT work as a regression
/// gate: verified empirically against this toolchain's own
/// `library/std/src/sys/process/unix/unix.rs::do_exec` (stable
/// aarch64-apple-darwin, and confirmed with a standalone probe binary)
/// that `setuid`/`setgid` to your OWN id is a permitted no-op with zero
/// observable effect, AND std's own `setgroups(0, NULL)` privilege-drop
/// call (triggered whenever `.uid()` is set without `.groups()`) silently
/// swallows `EPERM` for a non-root caller — so a child spawned with the
/// daemon's own uid/gid looks byte-for-byte identical to one spawned with
/// `spec.credentials: None`, whether or not `command.uid`/`command.gid`
/// were ever called. That version would pass unchanged even with both
/// lines deleted.
///
/// This version instead targets a uid/gid the test process does NOT own.
/// A non-root `setuid`/`setgid` to any id other than your own real/
/// effective/saved id fails `EPERM` (POSIX) — deterministically, on every
/// unix — so if `command.uid`/`command.gid` are actually invoked inside
/// `TokioRunner::spawn`'s child setup, `spawn()` itself returns `Err`
/// (std pipes a pre-exec failure back to the parent before ever calling
/// `execve`). If the two lines are deleted, `spec.credentials` is simply
/// never applied to the `Command` and `spawn()` succeeds instead. That
/// difference is exactly what each assertion below checks.
#[tokio::test]
async fn credentials_are_actually_applied_a_foreign_id_is_refused_by_the_os() {
    if nix::unistd::geteuid().is_root() {
        // As root, setuid/setgid to an arbitrary id typically succeeds
        // instead of failing — this test's whole premise (a guaranteed
        // EPERM) doesn't hold, and the root-only test below already
        // covers the apply path under privilege. Nothing to assert here.
        return;
    }
    let own_uid = nix::unistd::geteuid().as_raw();
    let own_gid = nix::unistd::getegid().as_raw();
    // Neither number needs to name a real passwd/group entry — a raw
    // setuid(2)/setgid(2) EPERMs on an unowned id regardless of whether
    // anything in /etc/passwd or /etc/group claims it.
    let foreign_uid = if own_uid == 1 { 2 } else { 1 };
    let foreign_gid = if own_gid == 1 { 2 } else { 1 };
    let runner = TokioRunner::new();

    // Isolates `command.uid(creds.uid)`: `gid: None` means the
    // `if let Some(gid) = creds.gid` line is never even reached, so a
    // failure here can only come from the unconditional `.uid()` call.
    let dir = tempfile::tempdir().unwrap();
    let mut uid_spec = spec_for(&dir, "id", &["-u"]);
    uid_spec.credentials = Some(Credentials {
        uid: foreign_uid,
        gid: None,
    });
    let uid_result = runner.spawn(&uid_spec);
    assert!(
        uid_result.is_err(),
        "spawning with a foreign uid must be refused by the OS if `command.uid` is really \
         called; an `Ok` here means the credentials were silently dropped on the floor"
    );

    // Isolates `command.gid(gid)`: `uid: own_uid` is a permitted no-op
    // (see this fn's own doc), so a failure here can only come from the
    // `if let Some(gid) = creds.gid { command.gid(gid); }` line.
    let dir = tempfile::tempdir().unwrap();
    let mut gid_spec = spec_for(&dir, "id", &["-g"]);
    gid_spec.credentials = Some(Credentials {
        uid: own_uid,
        gid: Some(foreign_gid),
    });
    let gid_result = runner.spawn(&gid_spec);
    assert!(
        gid_result.is_err(),
        "spawning with a foreign gid must be refused by the OS if `command.gid` is really \
         called; an `Ok` here means the credentials were silently dropped on the floor"
    );
}

#[tokio::test]
#[ignore = "needs root: run with `sudo -E cargo test -p shep-daemon --test real_runner -- --ignored`"]
async fn a_dropped_child_runs_as_the_requested_user() {
    // Real time: this file's whole tier is real OS behavior.
    assert!(
        nix::unistd::geteuid().is_root(),
        "this test only means anything as root"
    );
    let target = nix::unistd::User::from_name("nobody")
        .unwrap()
        .expect("every unix box has `nobody`");

    let dir = tempfile::tempdir().unwrap();
    // `id -G` after `id -u`: proves not just the uid drop but that std's
    // do_exec cleared root's supplementary groups (setgroups(0, NULL),
    // called automatically whenever `.uid()` is set without `.groups()` —
    // see tokio_runner.rs's comment) instead of leaking them into the
    // dropped-privilege child.
    let mut spec = spec_for(&dir, "/bin/sh", &["-c", "id -u; id -G"]);
    spec.credentials = Some(Credentials {
        uid: target.uid.as_raw(),
        gid: Some(target.gid.as_raw()),
    });

    let runner = TokioRunner::new();
    let (mut proc, mut io) = runner.spawn(&spec).unwrap();
    let uid_line = tokio::time::timeout(Duration::from_secs(5), io.logs.recv())
        .await
        .expect("the child must print its uid")
        .expect("the log pump must deliver the line");
    assert_eq!(uid_line.line.trim(), target.uid.as_raw().to_string());
    assert!(!uid_line.err);

    let groups_line = tokio::time::timeout(Duration::from_secs(5), io.logs.recv())
        .await
        .expect("the child must print its group list")
        .expect("the log pump must deliver the line");
    let groups: Vec<&str> = groups_line.line.split_whitespace().collect();
    assert_eq!(
        groups,
        vec![target.gid.as_raw().to_string().as_str()],
        "supplementary groups must be cleared, leaving only the target gid"
    );
    assert!(!groups_line.err);

    assert_eq!(proc.wait().await.code, Some(0));
}

/// RAII guard: sets `PATH` for the duration of a test and restores the
/// original value on drop (including on panic, so a failing assertion never
/// leaks a mutated `PATH` into whichever OTHER test the harness's default
/// multi-threaded runner happens to run concurrently).
///
/// # Why `unsafe` is contained to this file, not the `shep-daemon` crate
///
/// `std::env::set_var`/`remove_var` are `unsafe fn` (edition 2024): the
/// hazard they document is an OS thread doing a raw, std-unsynchronized
/// `getenv` at the same instant. `tests/real_runner.rs` is compiled as its
/// own crate root (a `[[test]]` binary), not part of the `shep-daemon`
/// library crate `lib.rs` gates with `#![deny(unsafe_code)]` — so this does
/// NOT add a second unsafe site to that crate's documented one. Within this
/// binary specifically: no other test here reads or writes `PATH`, and
/// `std::process::Command::spawn` (used throughout this file) reads env
/// through std's OWN `env_read_lock`/fork synchronization (see
/// `library/std/src/sys/process/unix/unix.rs`), not a raw unsynchronized
/// `getenv` — so the actual soundness hazard the `unsafe` marker exists for
/// doesn't apply to anything this binary does.
struct PathGuard {
    original: Option<String>,
}

impl PathGuard {
    fn set(new_path: &std::path::Path) -> Self {
        let original = std::env::var("PATH").ok();
        // SAFETY: see struct doc.
        unsafe { std::env::set_var("PATH", new_path) };
        Self { original }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        match &self.original {
            // SAFETY: see PathGuard's doc.
            Some(value) => unsafe { std::env::set_var("PATH", value) },
            // SAFETY: see PathGuard's doc.
            None => unsafe { std::env::remove_var("PATH") },
        }
    }
}

#[tokio::test]
async fn a_bare_interpreter_resolves_via_the_seeded_path() {
    // Originally written to stand in for Task 10's not-yet-created e2e tier
    // (see task-8-report.md); that tier now exists (`tests/daemon_e2e.rs`,
    // `a_bare_interpreter_resolves_via_the_inherited_path`) and re-proves
    // this same regression through the full daemon RPC stack — Start over
    // the real socket -> supervisor -> assemble() -> TokioRunner -> OS exec.
    // Kept here too rather than deleted: this test isolates the
    // assemble()+TokioRunner tier specifically (config -> assemble()'s
    // base_env() PATH seed -> TokioRunner spawn -> OS exec), so a failure
    // here versus one only in the e2e tier still tells you which layer
    // regressed.
    //
    // WHY a hand-rolled shim instead of `sh`/`node`: `/bin/sh` is reachable
    // even with a completely EMPTY child env, because glibc/libSystem's
    // `execvp` falls back to the OS's compiled-in default search path
    // (`_PATH_DEFPATH`, `/usr/bin:/bin` on macOS/BSD) whenever `PATH` is
    // ABSENT from the env it's given — independent of anything assemble()
    // does. Verified empirically: `subprocess.run(["sh", ...], env={})`
    // (a fully empty env, no PATH key at all) still succeeds. An earlier
    // version of this test used a bare "sh" and did NOT actually gate the
    // fix (reverting `base_env()` left it passing). A shim living in a
    // throwaway tempdir can NEVER be found by that OS-level fallback, so it
    // can only resolve if assemble()'s seeded PATH genuinely reaches it.
    use shep_core::config::{AppConfig, normalize};
    use shep_core::paths::ShepPaths;
    use shep_daemon::assemble::assemble;
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let shim_dir = dir.path().join("bin");
    fs::create_dir_all(&shim_dir).unwrap();
    let shim_path = shim_dir.join("shep-test-interp");
    fs::write(&shim_path, "#!/bin/sh\necho shim-exec-ok\n").unwrap();
    let mut perms = fs::metadata(&shim_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&shim_path, perms).unwrap();

    // Points the DAEMON's (this test process's) own PATH at ONLY the shim's
    // directory — no `/usr/bin:/bin`, nothing else — so base_env() has
    // exactly one place to find "shep-test-interp" and no coincidental
    // fallback can paper over a regression.
    let _path_guard = PathGuard::set(&shim_dir);

    let paths = ShepPaths {
        home: dir.path().to_path_buf(),
        daemon_config: dir.path().join("shep.toml"),
        snapshot: dir.path().join("flock.json"),
        logs: dir.path().join("logs"),
        pids: dir.path().join("pids"),
        run: dir.path().join("run"),
        socket: dir.path().join("run/shep.sock"),
        barks: dir.path().join("barks.jsonl"),
    };
    let app_config = AppConfig {
        name: "bare".to_string(),
        script: "unused".to_string(),
        args: vec![],
        interpreter: Some("shep-test-interp".to_string()), // bare: only found via seeded PATH
        ..Default::default()
    };
    let app = normalize(app_config).unwrap();
    let spec = assemble(&app, 0, &paths, None);
    assert_eq!(
        spec.program, "shep-test-interp",
        "sanity: genuinely bare, not accidentally absolute"
    );

    let runner = TokioRunner::new();
    let (mut proc, mut io) = runner.spawn(&spec).unwrap();
    let line = tokio::time::timeout(Duration::from_secs(5), io.logs.recv())
        .await
        .expect("the shim must resolve via the seeded PATH and produce output")
        .expect("logs channel closed before the line arrived");
    assert_eq!(line.line, "shim-exec-ok");
    assert!(!line.err);
    let outcome = proc.wait().await;
    assert_eq!(outcome.code, Some(0));
}
