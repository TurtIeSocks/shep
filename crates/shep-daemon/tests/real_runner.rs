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
use std::path::PathBuf;
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

    // By the time both lines were observed on `logs`, the pump's file write
    // for each already completed (write-then-send ordering in the runner) —
    // no extra sleep/poll needed to read these back reliably.
    assert_eq!(fs::read_to_string(&out_file).unwrap(), "out-line\n");
    assert_eq!(fs::read_to_string(&err_file).unwrap(), "err-line\n");
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
/// NOT add a third unsafe site to that crate's documented two. Within this
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
