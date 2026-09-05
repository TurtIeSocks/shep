//! Behavioral tests for [`shep_daemon::tokio_runner::TokioRunner`] against
//! real `/bin/sh` child processes.
//!
// Runs on the real, unpaused clock, in a separate binary from the
// paused-clock unit tests.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use shep_core::signals::OperatorSignal;
use shep_daemon::channel::{ChildMessage, ShepherdMessage};
use shep_daemon::privilege::Credentials;
use shep_daemon::runner::{
    AdoptSpec, AdoptedReaper, ProcIo, ProcessRunner, RunningProcess, SpawnSpec, StdinWrite,
    StopSignal,
};
use shep_daemon::tokio_runner::TokioRunner;

/// Builds a `/bin/sh -c <script>` spec writing logs into a fresh tempdir.
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
        stdin: false,
        credentials: None,
    }
}

/// Builds a spec running `program args...` with logs under `dir`, for an
/// arbitrary program rather than `sh_spec`'s single shell script.
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
        stdin: false,
        credentials: None,
    }
}

/// How long a log line gets to travel from the pump's `write_all` to the
/// file: slack for a loaded runner, not an expected duration.
const LOG_WRITE_DEADLINE: Duration = Duration::from_secs(5);

/// A log file's contents with the daemon's per-line timestamp stripped,
/// so an assertion is about what the sheep wrote.
///
/// A missing or unreadable file reads as empty, which is what a poll on
/// a file not yet created wants.
fn unstamped_file(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut out = String::new();
    for line in text.lines() {
        out.push_str(shep_core::logstamp::strip(line));
        out.push('\n');
    }
    out
}

/// Waits for `path` to hold exactly `expected`, failing at
/// [`LOG_WRITE_DEADLINE`].
///
/// Polls rather than sleeping a fixed guess, the same shape as
/// [`assert_reaped`]: bounded by what must eventually be true, not by a
/// guessed number.
async fn await_file_contents(path: &Path, expected: &str) {
    let settled = tokio::time::timeout(LOG_WRITE_DEADLINE, async {
        while unstamped_file(path) != expected {
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

    // A line on `logs` means the pump issued the write, not that it landed:
    // `write_all().await` returning only means queued in `tokio::fs`'s
    // buffer. Read the file back on a bounded poll instead of assuming it.
    await_file_contents(&out_file, "out-line\n").await;
    await_file_contents(&err_file, "err-line\n").await;
}

/// A shell fragment that blocks until `marker` exists, polling once a
/// second: `sleep`'s only portable argument is a whole number of seconds.
///
/// Lets the test control exactly when a child prints its next line,
/// without guessing at timing.
fn wait_for_marker(marker: &Path) -> String {
    format!("while [ ! -f {} ]; do sleep 1; done", marker.display())
}

/// Fails if a reopen leaves the pump writing into the renamed inode,
/// silently producing an empty live log forever under `create`-mode
/// rotation.
///
/// Checks both halves: the archive must stop growing and the live path
/// must get the new lines. A pump holding a second handle to the old file
/// would still pass a check of only one side.
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
    assert_eq!(unstamped_file(&out_file), "");
    assert_eq!(unstamped_file(&archive), "before\n");

    fs::write(&marker, "").unwrap();
    let line = tokio::time::timeout(LOG_WRITE_DEADLINE, io.logs.recv())
        .await
        .expect("the child's second line must arrive")
        .expect("logs closed before the second line");
    assert_eq!(line.line, "after");

    await_file_contents(&out_file, "after\n").await;
    assert_eq!(
        unstamped_file(&archive),
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

    // Gives the shell time to run `trap "" TERM` before we signal it, or
    // the signal can win the race and kill it via the untrapped default.
    tokio::time::sleep(Duration::from_millis(100)).await;
    proc.signal(StopSignal::Term).unwrap();

    // Gives the child a window to have exited if the signal wrongly
    // killed it despite the trap.
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

/// A real child, a real `kill(2)`, and the one assertion the scripted tier
/// cannot make: the signal reached the sheep, not the lamb it forked.
///
/// The wrapper traps SIGUSR1 and prints one word; the lamb traps it and
/// prints another. A group delivery prints both. Bounded: a signal that
/// reached nobody must fail the read rather than hang it.
#[tokio::test]
async fn a_process_signal_reaches_the_sheep_and_not_its_lamb() {
    let dir = tempfile::tempdir().unwrap();
    // The lamb announces itself once its trap is armed, so the test waits
    // for that instead of racing the fork.
    let script = r#"
        trap 'echo sheep-got-it' USR1
        ( trap 'echo lamb-got-it' USR1; echo lamb-ready; while :; do sleep 0.1; done ) &
        while :; do sleep 0.1; done
    "#;
    let spec = sh_spec(
        script,
        false,
        dir.path().join("out.log"),
        dir.path().join("err.log"),
    );
    let runner = TokioRunner::new();
    let (mut proc, mut io) = runner.spawn(&spec).unwrap();
    let _reaper = Reaper(vec![i32::try_from(proc.pid()).unwrap()]);

    let ready = tokio::time::timeout(Duration::from_secs(10), io.logs.recv())
        .await
        .expect("the lamb did not announce itself within 10s")
        .expect("log channel closed");
    assert_eq!(ready.line, "lamb-ready");

    proc.signal_process(OperatorSignal::Usr1).unwrap();

    let answer = tokio::time::timeout(Duration::from_secs(10), io.logs.recv())
        .await
        .expect("nothing answered the signal within 10s")
        .expect("log channel closed");
    assert_eq!(answer.line, "sheep-got-it");

    // And nothing else follows it. A group delivery would put `lamb-got-it` on
    // the same stream; a bounded read that times out is the proof it did not.
    let extra = tokio::time::timeout(Duration::from_secs(2), io.logs.recv()).await;
    assert!(
        extra.is_err(),
        "the lamb answered too: {extra:?} — the signal reached the group"
    );

    proc.kill_tree().unwrap();
}

/// How long the forked grandchild in
/// [`a_graceful_stop_reaches_a_forked_grandchild`] sleeps: longer than
/// [`REAP_DEADLINE`], so a pass proves the signal reached it rather than
/// that it exited on its own.
const ORPHAN_SLEEP_SECS: u32 = 30;

/// How long [`assert_reaped`] waits for a pid to leave the process table:
/// slack for a loaded runner, not an expected duration.
const REAP_DEADLINE: Duration = Duration::from_secs(5);

/// Last-resort net: kills any process left alive when a test panics, so a
/// failing assertion never leaks a real process into the rest of the run.
///
/// Fires only while panicking. On the success path the test has already
/// proven these pids are gone, and signalling one the OS may have recycled
/// is a hazard rather than a safety net.
///
/// SIGKILLs the whole process group (`-pid`, not `pid`): `TokioRunner`
/// spawns every child as its own group leader, so a leader-only signal
/// would miss a forked grandchild.
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

/// Polls `kill(pid, None)` for `ESRCH` rather than sleeping a fixed guess.
/// `kill(pid, None)` still returns `Ok` for a zombie, so only `ESRCH`
/// proves the process is gone rather than exited-but-unreaped.
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

/// The wrapper forks a long-lived child without `exec`, so a leader-only
/// signal can let the wrapper exit clean while the fork runs on,
/// reparented and untracked.
///
/// Only the grandchild's death tells the two behaviors apart: the wrapper
/// exits on `SIGTERM` either way.
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

    // Pins the property `-pid` signals depend on: without `process_group(0)`
    // the pid would lead no group at all.
    assert_eq!(
        nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(leader)))
            .unwrap()
            .as_raw(),
        leader,
        "a spawned sheep must lead its own process group"
    );

    // The wrapper prints `$!` only after forking, so receiving this line
    // proves the grandchild already exists.
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

    // Gives the shell time to fork its `sleep 5` child before kill_tree's
    // SIGKILL, or the fork can miss the group signal entirely.
    tokio::time::sleep(Duration::from_millis(100)).await;
    proc.kill_tree().unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), proc.wait())
        .await
        .expect("kill_tree should reap promptly");
    assert_eq!(outcome.signal, Some(9));
}

/// fails if a real child with a channel does not see `SHEP_CHANNEL_VERSION`
/// in its environment, from the `Command` through the exec to a plain
/// shell read.
///
/// Bounded by `await_file_contents`'s `LOG_WRITE_DEADLINE`: a version that
/// never arrives fails in seconds rather than hanging the whole binary.
#[tokio::test]
async fn a_child_with_a_channel_is_told_which_channel_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = spec_for(&dir, "/bin/sh", &["-c", "echo \"$SHEP_CHANNEL_VERSION\""]);
    // The variable rides with the channel, not with every spawn: an app with
    // no fd 3 has no channel to be told the version of.
    spec.channel = true;

    let runner = TokioRunner::new();
    let (_proc, _io) = runner.spawn(&spec).unwrap();

    await_file_contents(&dir.path().join("out.log"), "1\n").await;
}

/// fails if `SHEP_CHANNEL_VERSION` leaks into a child given no channel: an
/// app with no fd 3 must not be told it has one.
#[tokio::test]
async fn a_child_without_a_channel_is_told_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let spec = spec_for(&dir, "/bin/sh", &["-c", "echo \"[$SHEP_CHANNEL_VERSION]\""]);
    assert!(!spec.channel, "`spec_for` opens no channel by default");

    let runner = TokioRunner::new();
    let (_proc, _io) = runner.spawn(&spec).unwrap();

    await_file_contents(&dir.path().join("out.log"), "[]\n").await;
}

/// A `/bin/sh` child that answers exactly `rounds` shepherd-channel
/// messages with a reply naming which round it is, then exits 0.
///
/// `read -r line <&3` blocks; a failed read exits with a distinct status
/// instead of looping, so a broken channel ends the case promptly.
///
/// The round number is the child's own count of completed reads, not
/// anything parsed from the message: the shepherd sends in order, so
/// reply *n* still proves the child got through *n* reads.
fn channel_echo_script(rounds: u32) -> String {
    format!(
        r#"n=0
while [ "$n" -lt {rounds} ]; do
  read -r line <&3 || exit 3
  n=$((n + 1))
  printf '{{"kind":"action-reply","action":"round-%s","body":"ok"}}\n' "$n" >&3
done"#
    )
}

/// How long one shepherd-channel exchange gets: slack for a loaded
/// runner, not an expected duration.
const CHANNEL_DEADLINE: Duration = Duration::from_secs(5);

/// Sends one message and returns the child's reply.
///
/// Panics with the child's stderr rather than a bare timeout: a shell
/// `read` on a non-blocking descriptor prints `read error: 0: Resource
/// temporarily unavailable` there and nowhere else.
///
/// A send that fails is folded into the same report: it means the writer
/// task already saw the child's end close, the same failure a moment
/// earlier.
async fn channel_round_trip(io: &mut ProcIo, round: u32, err_file: &Path) -> ChildMessage {
    let name = format!("round-{round}");
    let delivered = io
        .to_child
        .send(ShepherdMessage::Action {
            name,
            params: None,
            // Derived from `round`: a distinct id per call keeps each
            // round's dispatch its own.
            id: u64::from(round),
        })
        .await
        .is_ok();
    let reply = if delivered {
        tokio::time::timeout(CHANNEL_DEADLINE, io.from_child.recv())
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    reply.unwrap_or_else(|| {
        panic!(
            "round {round}: no reply on the shepherd channel; child stderr: {:?}",
            fs::read_to_string(err_file).unwrap_or_default()
        )
    })
}

/// Fails if fd 3 reaches the child non-blocking: a plain `read <&3` gets
/// `EAGAIN` instead of parking, so a child waiting for the shepherd never
/// hears it.
///
/// Two round trips: the first can pass even against a non-blocking fd 3,
/// since the message is often already buffered before the child's first
/// `read`. Only the second puts the child at the read first, since
/// nothing is sent until the first reply arrives.
///
/// `shutdown_with_message` depends on this: it always reaches a child
/// already parked on a read, never one still starting up.
#[tokio::test]
async fn a_child_can_block_reading_the_shepherd_channel() {
    let dir = tempfile::tempdir().unwrap();
    let err_file = dir.path().join("err.log");
    let runner = TokioRunner::new();
    let spec = sh_spec(
        &channel_echo_script(2),
        true,
        dir.path().join("out.log"),
        err_file.clone(),
    );

    let (mut proc, mut io) = runner.spawn(&spec).unwrap();
    let _reaper = Reaper(vec![i32::try_from(proc.pid()).unwrap()]);

    let first = channel_round_trip(&mut io, 1, &err_file).await;
    assert_eq!(
        first,
        ChildMessage::ActionReply {
            action: "round-1".to_string(),
            body: "ok".to_string(),
            // The echo script never writes an `id` key.
            id: None,
        }
    );

    let second = channel_round_trip(&mut io, 2, &err_file).await;
    assert_eq!(
        second,
        ChildMessage::ActionReply {
            action: "round-2".to_string(),
            body: "ok".to_string(),
            id: None,
        }
    );

    let outcome = tokio::time::timeout(REAP_DEADLINE, proc.wait())
        .await
        .expect("the child exits once it has answered both rounds");
    assert_eq!(
        outcome.code,
        Some(0),
        "status 3 is the script's own `read` failure"
    );
}

/// Proves `TokioRunner::spawn`'s `command.uid`/`command.gid` calls are
/// actually invoked, without requiring root.
///
/// Spawning with your own uid/gid does not work as a regression gate:
/// it is a permitted no-op, and std silently swallows the `EPERM` from
/// its own privilege-drop `setgroups` call for a non-root caller. A
/// child spawned this way looks the same whether or not the lines run.
///
/// This targets a uid/gid the test process does not own instead. A
/// non-root `setuid`/`setgid` to any other id fails `EPERM`
/// deterministically, so real calls make `spawn()` return `Err`; deleted
/// lines let `spawn()` succeed instead.
#[tokio::test]
async fn credentials_are_actually_applied_a_foreign_id_is_refused_by_the_os() {
    if nix::unistd::geteuid().is_root() {
        // As root, setuid/setgid to an arbitrary id typically succeeds, so
        // this test's EPERM premise doesn't hold.
        return;
    }
    let own_uid = nix::unistd::geteuid().as_raw();
    let own_gid = nix::unistd::getegid().as_raw();
    // Neither id needs a real passwd/group entry: setuid(2)/setgid(2)
    // EPERMs on any unowned id regardless.
    let foreign_uid = if own_uid == 1 { 2 } else { 1 };
    let foreign_gid = if own_gid == 1 { 2 } else { 1 };
    let runner = TokioRunner::new();

    // Isolates `command.uid`: `gid: None` means `command.gid` is never
    // reached, so a failure here can only come from `.uid()`.
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

    // Isolates `command.gid`: `uid: own_uid` is a permitted no-op, so a
    // failure here can only come from `.gid()`.
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
    assert!(
        nix::unistd::geteuid().is_root(),
        "this test only means anything as root"
    );
    let target = nix::unistd::User::from_name("nobody")
        .unwrap()
        .expect("every unix box has `nobody`");

    let dir = tempfile::tempdir().unwrap();
    // `id -G` after `id -u` proves the drop also cleared root's
    // supplementary groups instead of leaking them into the child.
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

/// RAII guard: sets `PATH` for a test and restores it on drop, including on
/// panic, so a failing assertion never leaks a mutated `PATH` into another
/// test running concurrently.
///
/// `set_var`/`remove_var` are `unsafe fn` in edition 2024: the hazard is
/// unsynchronized env reads from another thread. This binary is its own
/// crate root, separate from `shep-daemon`'s `#![deny(unsafe_code)]`; no
/// other test here touches `PATH`, and `Command::spawn` reads env through
/// std's own synchronized path, so nothing here can race it.
struct PathGuard {
    original: Option<String>,
}

impl PathGuard {
    fn set(new_path: &std::path::Path) -> Self {
        let original = std::env::var("PATH").ok();
        // SAFETY: no other test writes `PATH`; `Command::spawn`'s concurrent
        // reads go through std's synchronized path, not a raw `getenv`.
        unsafe { std::env::set_var("PATH", new_path) };
        Self { original }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        match &self.original {
            // SAFETY: no other test writes `PATH`; `Command::spawn`'s
            // concurrent reads go through std's synchronized path.
            Some(value) => unsafe { std::env::set_var("PATH", value) },
            // SAFETY: no other test writes `PATH`; `Command::spawn`'s
            // concurrent reads go through std's synchronized path.
            None => unsafe { std::env::remove_var("PATH") },
        }
    }
}

#[tokio::test]
async fn a_bare_interpreter_resolves_via_the_seeded_path() {
    // Isolates assemble()+TokioRunner from the full e2e stack. A shim, not
    // `/bin/sh`: `/bin/sh` falls back to the OS's default PATH even with
    // an empty env, so it would pass without a real seed. A shim in a
    // tempdir can only resolve through the seed.
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

    // Points this process's own PATH at only the shim's directory, so
    // base_env() has exactly one place to find "shep-test-interp".
    let _path_guard = PathGuard::set(&shim_dir);

    let paths = ShepPaths {
        home: dir.path().to_path_buf(),
        daemon_config: dir.path().join("shep.toml"),
        dogs_config: dir.path().join("dogs.toml"),
        snapshot: dir.path().join("flock.json"),
        logs: dir.path().join("logs"),
        pids: dir.path().join("pids"),
        run: dir.path().join("run"),
        socket: dir.path().join("run/shep.sock"),
        barks: dir.path().join("barks.jsonl"),
        kv: dir.path().join("kv.json"),
        overrides: dir.path().join("overrides.json"),
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

/// A real pipe on a real fd 0. `cat` echoes what is written to it, so a
/// line on stdout proves the whole path worked: created, mapped to fd 0,
/// written, flushed, and read by the child.
///
/// Bounded: a line that never arrives must fail this test, not hang it.
#[tokio::test]
async fn a_real_child_reads_a_line_written_to_its_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = spec_for(&dir, "/bin/cat", &[]);
    spec.stdin = true;
    let runner = TokioRunner::new();
    let (_proc, mut io) = runner.spawn(&spec).unwrap();

    let (done, ack) = tokio::sync::oneshot::channel();
    io.to_stdin
        .send(StdinWrite {
            line: "hello sheep".to_string(),
            done,
        })
        .await
        .unwrap();
    ack.await.unwrap().unwrap();

    let line = tokio::time::timeout(Duration::from_secs(10), io.logs.recv())
        .await
        .expect("no stdout line within 10s")
        .expect("log channel closed");
    assert!(!line.err);
    assert_eq!(line.line, "hello sheep");
}

/// fails if a spec that did not ask for a pipe gets one anyway. `/dev/null`
/// on fd 0 is the default; `cat` reading EOF immediately, rather than
/// waiting, is the observable difference.
#[tokio::test]
async fn a_child_that_did_not_ask_for_stdin_gets_eof_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = spec_for(&dir, "/bin/cat", &[]);
    spec.stdin = false;
    let runner = TokioRunner::new();
    let (mut proc, io) = runner.spawn(&spec).unwrap();

    assert!(io.to_stdin.is_closed());
    let outcome = tokio::time::timeout(Duration::from_secs(10), proc.wait())
        .await
        .expect("cat did not exit on EOF within 10s");
    assert_eq!(outcome.code, Some(0));
}

// --- Adoption: the seam a successor reaches a still-running sheep through ---
//
// Spawned via `std::process::Command`, so tokio holds no `Child`; only
// `AdoptedReaper`'s targeted `waitpid` collects the exit.

/// A child the way an adopted sheep reaches a successor: no `tokio::Child`
/// anywhere, so nothing but the reaper will ever wait on it.
fn adopted_child(script: &str) -> std::process::Child {
    std::process::Command::new("/bin/sh")
        .args(["-c", script])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn a shell")
}

/// The plainest adoption: a pid and its two log paths, no carried handles.
///
/// Every handle is `None` here, which is the shape a sheep whose log opens
/// had failed arrives in. The cases that are about handles fill them in.
fn adopt_spec(dir: &tempfile::TempDir, pid: u32, reaper: &Arc<AdoptedReaper>) -> AdoptSpec {
    AdoptSpec {
        pid,
        out_file: dir.path().join("out.log"),
        err_file: dir.path().join("err.log"),
        out_pipe: None,
        err_pipe: None,
        out_log: None,
        err_log: None,
        stdin_pipe: None,
        channel: None,
        reaper: Arc::clone(reaper),
    }
}

/// fails if an adopted sheep's real exit never reaches the supervisor.
///
/// Proves the reaper is reachable through `RunningProcess`, the type the
/// supervisor actually holds, so an adopted sheep's exit drives
/// autorestart and the EXIT column like a spawned one's does.
#[tokio::test]
#[expect(
    clippy::zombie_processes,
    reason = "the reaper collects these statuses; a Child::wait would take them first"
)]
async fn an_adopted_proc_reports_its_real_exit() {
    let dir = tempfile::tempdir().unwrap();
    let child = adopted_child("exit 3");
    let pid = child.id();
    let reaper = Arc::new(AdoptedReaper::new());

    let (mut proc, io) = TokioRunner::new()
        .adopt(adopt_spec(&dir, pid, &reaper))
        .expect("the real runner must be able to adopt");
    assert_eq!(
        proc.pid(),
        pid,
        "an adopted proc keeps the pid it was given"
    );

    let outcome = tokio::time::timeout(Duration::from_secs(10), proc.wait())
        .await
        .expect("the adopted pid must be reaped within the budget");
    assert_eq!((outcome.code, outcome.signal), (Some(3), None));
    drop(io);
}

/// fails if the adopted arm disturbed the path every sheep takes.
///
/// `wait` now chooses between two sources of an exit; the spawned one is
/// the choice every running flock depends on.
#[tokio::test]
async fn a_spawned_proc_still_reports_its_real_exit() {
    let dir = tempfile::tempdir().unwrap();
    let runner = TokioRunner::new();
    let (mut proc, io) = runner
        .spawn(&spec_for(&dir, "/bin/sh", &["-c", "exit 4"]))
        .unwrap();

    let outcome = tokio::time::timeout(Duration::from_secs(10), proc.wait())
        .await
        .expect("a spawned child must be waited within the budget");
    assert_eq!((outcome.code, outcome.signal), (Some(4), None));
    drop(io);
}

/// fails if an adopted sheep killed by a signal reports a code instead.
///
/// `output/rows.rs`'s `exit_cell` renders a code and a signal differently,
/// so collapsing the two makes the EXIT column lie about how a sheep died.
#[tokio::test]
#[expect(
    clippy::zombie_processes,
    reason = "the reaper collects these statuses; a Child::wait would take them first"
)]
async fn an_adopted_proc_reports_a_signal_as_a_signal() {
    let dir = tempfile::tempdir().unwrap();
    let child = adopted_child("sleep 30");
    let pid = child.id();
    let reaper = Arc::new(AdoptedReaper::new());

    let (mut proc, io) = TokioRunner::new()
        .adopt(adopt_spec(&dir, pid, &reaper))
        .expect("the real runner must be able to adopt");
    // Per-process rather than `kill_tree`: a child spawned here is in the
    // test binary's group, not its own, so there is no group to address.
    // `signal_process` targets the pid instead.
    proc.signal_process(OperatorSignal::Kill)
        .expect("SIGKILL the adopted sheep");

    let outcome = tokio::time::timeout(Duration::from_secs(10), proc.wait())
        .await
        .expect("the adopted pid must be reaped within the budget");
    assert_eq!((outcome.code, outcome.signal), (None, Some(9)));
    drop(io);
}

/// fails if an adopted sheep's output stops reaching its log file.
///
/// The carried pipe read end must feed the same pump a spawn feeds, and
/// the carried log handle must be written through rather than reopened.
/// The line already in the file proves the second half: a handle that
/// lost `O_APPEND` would overwrite it.
#[tokio::test]
#[expect(
    clippy::zombie_processes,
    reason = "the reaper collects these statuses; a Child::wait would take them first"
)]
async fn an_adopted_pump_appends_the_carried_pipes_lines_through_the_carried_handle() {
    let dir = tempfile::tempdir().unwrap();
    let out_file = dir.path().join("out.log");
    fs::write(&out_file, "before-the-handover\n").unwrap();

    let mut child = adopted_child("echo after-the-handover");
    let pid = child.id();
    let stdout = child.stdout.take().expect("piped stdout");
    let carried_pipe = tokio::net::unix::pipe::Receiver::from_file(std::fs::File::from(
        std::os::fd::OwnedFd::from(stdout),
    ))
    .expect("a child's stdout is a readable pipe");
    let carried_log = tokio::fs::OpenOptions::new()
        .append(true)
        .open(&out_file)
        .await
        .expect("open the log the predecessor was appending to");

    let reaper = Arc::new(AdoptedReaper::new());
    let mut spec = adopt_spec(&dir, pid, &reaper);
    spec.out_file = out_file.clone();
    spec.out_pipe = Some(carried_pipe);
    spec.out_log = Some(carried_log);

    let (mut proc, mut io) = TokioRunner::new()
        .adopt(spec)
        .expect("the real runner must be able to adopt");

    let line = tokio::time::timeout(Duration::from_secs(10), io.logs.recv())
        .await
        .expect("the carried pipe must still be pumped")
        .expect("the pump must forward the line");
    assert_eq!(line.line, "after-the-handover");
    await_file_contents(&out_file, "before-the-handover\nafter-the-handover\n").await;

    let outcome = tokio::time::timeout(Duration::from_secs(10), proc.wait())
        .await
        .expect("the adopted pid must be reaped within the budget");
    assert_eq!(outcome.code, Some(0));
}

/// A `/bin/sh` child that holds the far end of a socketpair on fd 3 and
/// echoes each shepherd message back once, like [`channel_echo_script`]
/// for a spawned sheep.
///
/// Spawned with `command-fds`, not `TokioRunner::spawn`: the case below is
/// about the successor's half, so the daemon side of the socketpair has
/// to be one this test owns rather than one the runner's pumps hold.
///
/// A `std::process::Command`, so tokio holds no `Child`; only the reaper
/// ever waits it, the same reason [`adopted_child`] uses one.
fn child_holding_a_channel(child_end: std::os::fd::OwnedFd, rounds: u32) -> std::process::Child {
    use command_fds::{CommandFdExt as _, FdMapping};

    let mut command = std::process::Command::new("/bin/sh");
    command.args(["-c", &channel_echo_script(rounds)]);
    command
        .fd_mappings(vec![FdMapping {
            parent_fd: child_end,
            child_fd: 3,
        }])
        .expect("map the socketpair onto the child's fd 3");
    command.spawn().expect("spawn a shell holding fd 3")
}

/// fails if an adopted shepherd channel does not reach a real app blocked
/// on `read -r line <&3`.
///
/// Two things only a real process, not a socket pair, can check. The
/// child is a separate open file description, so `set_nonblocking(true)`
/// on the daemon's end must not reach it, or the child's `read` would get
/// `EAGAIN` and die with status 3. And the reply must come back through
/// the reader task the adoption rebuilt.
#[tokio::test]
#[expect(
    clippy::zombie_processes,
    reason = "the reaper collects this status; a Child::wait would take it first"
)]
async fn an_adopted_channel_reaches_a_real_child_that_blocks_on_fd_3() {
    let dir = tempfile::tempdir().unwrap();
    let (daemon_end, child_end) = std::os::unix::net::UnixStream::pair().unwrap();
    // Cleared for the child's end exactly as the spawn path clears it: both
    // ends come back non-blocking from `pair()`, and a shell `read` on one
    // fails instead of parking.
    child_end.set_nonblocking(false).unwrap();
    let child = child_holding_a_channel(std::os::fd::OwnedFd::from(child_end), 1);
    let pid = child.id();

    daemon_end.set_nonblocking(true).unwrap();
    let daemon_end = tokio::net::UnixStream::from_std(daemon_end).unwrap();
    let reaper = Arc::new(AdoptedReaper::new());
    let mut spec = adopt_spec(&dir, pid, &reaper);
    spec.channel = Some(daemon_end);

    let (mut proc, mut io) = TokioRunner::new()
        .adopt(spec)
        .expect("the real runner must be able to adopt");

    io.to_child
        .send(ShepherdMessage::Action {
            name: "round-1".to_string(),
            params: None,
            id: 1,
        })
        .await
        .expect("an adopted sheep must still have a channel writer");
    let reply = tokio::time::timeout(CHANNEL_DEADLINE, io.from_child.recv())
        .await
        .expect("the child must answer over the adopted channel")
        .expect("the adopted reader must forward the reply");
    assert_eq!(
        reply,
        ChildMessage::ActionReply {
            action: "round-1".to_string(),
            body: "ok".to_string(),
            id: None,
        }
    );

    let outcome = tokio::time::timeout(REAP_DEADLINE, proc.wait())
        .await
        .expect("the adopted pid must be reaped within the budget");
    assert_eq!(
        outcome.code,
        Some(0),
        "status 3 is the child's own `read` failing, which is a non-blocking fd 3"
    );
}
