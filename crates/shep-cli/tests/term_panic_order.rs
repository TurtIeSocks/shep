//! Bounded, headless proof of `lookout::term::install_panic_hook`'s
//! ordering: `restore()` runs BEFORE the previous (default) hook prints its
//! backtrace. That ordering is the module's entire reason to exist — a
//! crash that leaves raw mode and the alternate screen active is, per
//! `term.rs`'s own doc, the worst failure a TUI can have.
//!
//! This file is the permanent check that catches a future refactor swapping
//! the two statements back.
//!
//! **How the ordering becomes observable without a real terminal or a
//! pty.** `restore()` writes its `LeaveAlternateScreen`/`Show` escapes to
//! `io::stdout()`; the default panic hook (chained after `restore()` by
//! `install_panic_hook`) writes the backtrace to `io::stderr()`. Two
//! separate pipes captured independently (`Command::output`'s ordinary
//! behaviour) would not preserve which write happened first. Redirecting
//! BOTH file descriptors to clones of the same open file makes the OS
//! serialize every write to that file in true chronological order — the
//! same guarantee a shell's `2>&1` gives on a real terminal, without a pty.
//!
//! `#![cfg(unix)]` for the same reason `cli_e2e.rs` carries it: this file
//! is its own compilation unit, and without the guard `--all-targets` would
//! build it (uselessly, since `lookout` itself is `#[cfg(unix)]`) on the
//! Windows CI leg.

#![cfg(unix)]

use std::fs::File;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::cargo::CommandCargoExt as _;

/// A deliberate panic in a process doing nothing but installing a hook has
/// no legitimate reason to take more than a fraction of a second; five
/// seconds is generous headroom, not a tight bound. Enforced by hand below
/// (`wait_bounded`) rather than via `assert_cmd`'s own `.timeout()`, which
/// only applies to its `.output()`/`.assert()` paths — this test needs
/// `.status()` so the custom stdout/stderr redirection below survives
/// unmolested (`Command::output` unconditionally overwrites both to fresh
/// pipes before spawning).
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// fails if `install_panic_hook`'s two statements — `restore()`, then the
/// previous hook — are ever swapped back. Runs the real `shep` binary with
/// `SHEP_TERM_PANIC_PROBE` set, which calls `install_panic_hook()` and
/// panics on purpose (`lookout::term::probe_panic_for_test`); asserts the
/// `LeaveAlternateScreen` escape (`\x1b[?1049l`) appears, byte-for-byte,
/// before the panic backtrace text in the merged stdout+stderr stream.
///
/// Proven to actually catch the regression it names: swapping
/// `install_panic_hook`'s two statements (restored via `cp` from a
/// pre-mutation snapshot, never `git checkout`) reddened this exact
/// assertion before the fix landed — see the task report for the byte
/// offsets observed.
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
/// `Command::status()` directly, so a probe that somehow hangs fails this
/// test with a named panic instead of relying on the harness's own process
/// timeout (which fails the whole binary and names nothing — the same
/// distinction IR-46 draws for `await`s).
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
