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
    let mut spec = spec_for(&dir, "/bin/sh", &["-c", "id -u"]);
    spec.credentials = Some(Credentials {
        uid: target.uid.as_raw(),
        gid: Some(target.gid.as_raw()),
    });

    let runner = TokioRunner::new();
    let (mut proc, mut io) = runner.spawn(&spec).unwrap();
    let printed = tokio::time::timeout(Duration::from_secs(5), io.logs.recv())
        .await
        .expect("the child must print its uid")
        .expect("the log pump must deliver the line");
    assert_eq!(printed.line.trim(), target.uid.as_raw().to_string());
    assert!(!printed.err);
    assert_eq!(proc.wait().await.code, Some(0));
}

#[tokio::test]
async fn a_bare_interpreter_resolves_via_the_seeded_path() {
    // Standing in for Task 10's not-yet-created e2e tier (this crate has no
    // `tests/e2e_*.rs` yet — see task-8-report.md): proves the FULL chain
    // (config -> assemble()'s base_env() PATH seed -> TokioRunner spawn ->
    // OS exec) actually resolves a BARE program name, not just an absolute
    // one. Every other test in this file spawns `/bin/sh` by absolute path,
    // which never exercises PATH lookup at all — exactly what masked
    // adversarial finding #1 (assemble() built `spec.env` with no PATH, so
    // `env_clear()` + `envs(&spec.env)` handed a bare interpreter an empty
    // env and every such spawn ENOENT'd).
    use shep_core::config::{AppConfig, normalize};
    use shep_core::paths::ShepPaths;
    use shep_daemon::assemble::assemble;

    let dir = tempfile::tempdir().unwrap();
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
        script: "-c".to_string(),
        args: vec!["echo bare-exec-ok".to_string()],
        interpreter: Some("sh".to_string()), // bare: resolved via PATH, never /bin/sh directly
        ..Default::default()
    };
    let app = normalize(app_config).unwrap();
    let spec = assemble(&app, 0, &paths, None);
    assert_eq!(
        spec.program, "sh",
        "sanity: genuinely bare, not accidentally absolute"
    );

    let runner = TokioRunner::new();
    let (mut proc, mut io) = runner.spawn(&spec).unwrap();
    let line = tokio::time::timeout(Duration::from_secs(5), io.logs.recv())
        .await
        .expect("a bare `sh` must resolve via the seeded PATH and produce output")
        .expect("logs channel closed before the line arrived");
    assert_eq!(line.line, "bare-exec-ok");
    assert!(!line.err);
    let outcome = proc.wait().await;
    assert_eq!(outcome.code, Some(0));
}
