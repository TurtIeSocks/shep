//! Proves `lookout::term::install_panic_hook`'s ordering: `restore()`
//! runs before the previous hook prints its backtrace. A crash leaving
//! raw mode and the alternate screen active is a TUI's worst failure.
//!
//! `restore()` writes to `io::stdout()`; the default hook writes to
//! `io::stderr()`. Redirecting both to clones of one open file makes
//! the OS serialize the writes. Same guarantee a shell's `2>&1` gives,
//! without a pty.
//!
//! `#![cfg(unix)]`: `lookout` is unix only, so `--all-targets` would
//! otherwise build this file on Windows.

#![cfg(unix)]

use std::fs::File;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::cargo::CommandCargoExt as _;

/// Five seconds: generous headroom for a hook install and panic, not a
/// tight bound. Enforced by hand (`wait_bounded`) rather than
/// `assert_cmd`'s `.timeout()`, which needs `.output()`/`.assert()`.
/// That would overwrite the custom stdout/stderr redirection below.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Runs the real `shep` binary with `SHEP_TERM_PANIC_PROBE` set, which
/// calls `install_panic_hook()` and panics
/// (`lookout::term::probe_panic_for_test`). Asserts the
/// `LeaveAlternateScreen` escape (`\x1b[?1049l`) appears before the
/// panic backtrace in the merged stdout+stderr stream.
#[test]
fn the_restore_escape_lands_before_the_panic_backtrace() {
    let dir = tempfile::tempdir().expect("tempdir for the probe's merged output");
    let path = dir.path().join("probe.out");
    let sink = File::create(&path).expect("create probe output file");
    let sink_for_stderr = sink.try_clone().expect("clone probe output handle");

    let mut cmd = Command::cargo_bin("shep").expect("locate the built shep binary");
    cmd.env("SHEP_TERM_PANIC_PROBE", "1")
        .env("RUST_BACKTRACE", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::from(sink))
        .stderr(Stdio::from(sink_for_stderr));

    let status = wait_bounded(cmd, PROBE_TIMEOUT);
    assert!(
        !status.success(),
        "the probe is supposed to panic, not exit cleanly"
    );

    let merged = std::fs::read(&path).expect("read the probe's merged output");
    let restore_at = find(&merged, b"\x1b[?1049l").expect("the restore escape must appear at all");
    let panic_at = find(&merged, b"panicked at").expect("the panic backtrace must appear at all");

    assert!(
        restore_at < panic_at,
        "restore escape at byte {restore_at} must precede the panic backtrace at byte \
         {panic_at}; merged output:\n{}",
        String::from_utf8_lossy(&merged)
    );
}

/// Spawns `cmd` and polls for its exit rather than blocking on
/// `Command::status()`. A hung probe then fails with a named panic.
/// The harness's own process timeout would instead fail the whole
/// binary and name nothing.
fn wait_bounded(mut cmd: Command, timeout: Duration) -> std::process::ExitStatus {
    let mut child = cmd.spawn().expect("spawn the probe subprocess");
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("poll the probe subprocess") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("probe subprocess did not exit within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
