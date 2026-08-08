//! `connect_or_spawn`/`connect_or_spawn_with`: the autostart path, driven
//! against the hand-rolled daemon fakes in [`shep_client::testing`] plus a
//! handful of real child processes.
//!
//! An integration test rather than a `#[cfg(test)] mod tests` block inside
//! `spawn.rs`, for the reason spelled out at the top of `request_reply.rs`.

#![cfg(unix)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use shep_client::ConnectError;
use shep_client::spawn::{SpawnError, SpawnOutcome, connect_or_spawn, connect_or_spawn_with};
use shep_client::testing::{
    child_exiting_with, fake_daemon, fast_opts, sample_ack, start_fake_daemon_answering_on,
};
use shep_core::protocol::{RpcError, RpcErrorCode};

/// Reaps the `cat` children the launcher closures spawn.
///
/// `connect_or_spawn_with` owns each `Child` and drops it on the way out —
/// which closes the pipe and lets `cat` exit — but it never `wait()`s, and it
/// must not: in production that child IS the daemon, and waiting on it would
/// hang the CLI forever. Nothing else reaps them either, so without this every
/// `cat` stays a zombie for the life of the test binary.
#[derive(Debug, Default)]
struct Reaper(Arc<Mutex<Vec<i32>>>);

impl Reaper {
    /// A launcher that spawns a child which outlives the call and then dies on
    /// its own, with no sleep and no orphan: `cat` blocks reading a piped
    /// stdin whose write end is owned by the `Child`. When
    /// `connect_or_spawn_with` drops that `Child`, the pipe closes, `cat` sees
    /// EOF and exits. Lifetime is tied exactly to the call under test — a
    /// `sleep 60` would leak past it, and Phase 2b already paid for that
    /// lesson (`daemon_e2e.rs:118-138`).
    fn spawn_long_lived(
        &self,
    ) -> impl FnOnce() -> std::io::Result<std::process::Child> + Send + 'static {
        let pids = Arc::clone(&self.0);
        move || {
            let child = std::process::Command::new("cat")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()?;
            pids.lock()
                .unwrap()
                .push(i32::try_from(child.id()).unwrap());
            Ok(child)
        }
    }
}

impl Drop for Reaper {
    fn drop(&mut self) {
        for pid in self.0.lock().unwrap().drain(..) {
            let _ = nix::sys::wait::waitpid(nix::unistd::Pid::from_raw(pid), None);
        }
    }
}

#[tokio::test]
async fn an_existing_daemon_is_used_without_launching_anything() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let _served = fake_daemon(&path, Ok(sample_ack())).await; // Task 1's helper

    let outcome = connect_or_spawn_with(
        &path,
        || unreachable!("a live daemon must never be re-spawned"),
        fast_opts(),
    )
    .await
    .unwrap();
    assert!(matches!(outcome, SpawnOutcome::Connected(_)));
}

/// THE load-bearing test of this task.
///
/// The launcher does what a real cold start does: it makes a socket appear
/// that is BOUND but never accepted from — a daemon that has reached
/// `boot.rs:498` and not `boot.rs:707` — and returns a child that STAYS
/// ALIVE for the whole call. Both halves matter. If the child exited, the
/// dead-child fast path would short-circuit before any probe ran, and the
/// bare-`connect()` implementation this test exists to catch would pass.
#[tokio::test]
async fn a_socket_that_accepts_but_never_handshakes_is_not_mistaken_for_ready() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let reaper = Reaper::default();
    // The listener must outlive the closure; park it where the test owns it.
    // `std`'s listener, not tokio's: the launcher is a sync `FnOnce`, and
    // this one must never accept anyway.
    let held: Arc<Mutex<Option<std::os::unix::net::UnixListener>>> = Arc::default();

    let err = {
        let slot = Arc::clone(&held);
        let bind_at = path.clone();
        let long_lived = reaper.spawn_long_lived();
        connect_or_spawn_with(
            &path,
            move || {
                *slot.lock().unwrap() = Some(std::os::unix::net::UnixListener::bind(&bind_at)?);
                long_lived()
            },
            fast_opts(),
        )
        .await
        .unwrap_err()
    };

    let SpawnError::DeadlineExpired {
        last: Some(ConnectError::HandshakeTimeout { .. }),
        after,
    } = err
    else {
        panic!("a backlogged connect must read as an unfinished handshake, got {err:?}");
    };
    assert_eq!(after, fast_opts().deadline);
    assert!(
        held.lock().unwrap().is_some(),
        "the fixture must actually have bound the socket"
    );
}

#[tokio::test]
async fn a_child_that_dies_fails_fast_instead_of_waiting_out_the_deadline() {
    let dir = tempfile::tempdir().unwrap();
    // Nothing ever binds here, so the first probe fails `Connect` and the
    // launcher runs. `child_exiting_with(3)` returns an already-doomed child.
    let absent_path = dir.path().join("absent.sock");

    let started = Instant::now();
    let err = connect_or_spawn(&absent_path, || child_exiting_with(3))
        .await
        .unwrap_err();
    let SpawnError::DaemonExited { status } = err else {
        panic!("a dead child's status must reach the caller, got {err:?}");
    };
    assert_eq!(status.code(), Some(3));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "must not wait out SPAWN_DEADLINE on a dead child"
    );
}

/// The losing side of a cold-start race (fix G). The launcher starts a child
/// that immediately exits 10 AND brings up a daemon that answers — exactly
/// what happens when another `shep` process won the `flock(2)`. Treating any
/// non-zero status as fatal fails this test.
#[tokio::test]
async fn a_child_exiting_with_the_already_running_code_keeps_probing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let answer_on = path.clone();

    let outcome = connect_or_spawn_with(
        &path,
        move || {
            // binds AND accepts; the handle is detached deliberately — the
            // fake outlives the launcher closure and dies with the runtime.
            start_fake_daemon_answering_on(&answer_on);
            let child = std::process::Command::new("sh")
                .args(["-c", "exit 10"])
                .spawn()?;
            // Without this, the loop's first probe can win the race against
            // `try_wait()` ever observing this child's exit at all — the
            // fake daemon started above is already answering, so a probe
            // that lands before the next `try_wait()` succeeds and the test
            // passes without the exit-10 special case ever running. Verified
            // by mutation: replacing the `DAEMON_ALREADY_RUNNING` check below
            // with `if true` (treat any exit as fatal) still left this test
            // green without the sleep.
            std::thread::sleep(Duration::from_millis(50));
            Ok(child)
        },
        fast_opts(),
    )
    .await
    .unwrap();
    assert!(
        matches!(outcome, SpawnOutcome::Spawned(_)),
        "another process winning the race is not this process's failure"
    );
}

#[tokio::test]
async fn a_protocol_mismatch_propagates_instead_of_spawning_a_second_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let _served = fake_daemon(
        &path,
        Err(RpcError {
            code: RpcErrorCode::ProtocolMismatch,
            message: "daemon speaks protocol 2, client speaks 1".into(),
        }),
    )
    .await;

    let err = connect_or_spawn_with(
        &path,
        || unreachable!("a refusing daemon is still a daemon"),
        fast_opts(),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        SpawnError::Connect(ConnectError::ProtocolMismatch { .. })
    ));
}

/// The same propagate-immediately rule as the test above, but for a mismatch
/// that only a *loop* probe observes — reachable via the no-launch
/// `HandshakeTimeout` branch hitting a daemon that is still mid-boot on the
/// first probe and only answers (with a refusal) on a later one. Folding
/// this into `last` and looping to the deadline would misdiagnose a
/// definitively-answered condition as "daemon unreachable".
#[tokio::test]
async fn a_protocol_mismatch_on_a_loop_probe_propagates_instead_of_looping_to_the_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let reaper = Reaper::default();
    let answer_on = path.clone();

    let started = Instant::now();
    let err = {
        let long_lived = reaper.spawn_long_lived();
        connect_or_spawn_with(
            &path,
            move || {
                // Nothing is bound yet when this launcher runs — the first
                // probe already saw `ConnectError::Connect`. Bind the
                // refusing fake here so it's a *loop* probe, not the first
                // one, that observes the mismatch.
                tokio::runtime::Handle::current().block_on(fake_daemon(
                    &answer_on,
                    Err(RpcError {
                        code: RpcErrorCode::ProtocolMismatch,
                        message: "daemon speaks protocol 2, client speaks 1".into(),
                    }),
                ));
                long_lived()
            },
            fast_opts(),
        )
        .await
        .unwrap_err()
    };

    assert!(
        matches!(
            err,
            SpawnError::Connect(ConnectError::ProtocolMismatch { .. })
        ),
        "a mismatch on a loop probe must propagate immediately, got {err:?}"
    );
    assert!(
        started.elapsed() < fast_opts().deadline,
        "must not burn the rest of the deadline after a definitive answer"
    );
}

/// A daemon in the bind->serve gap must not provoke a second daemon.
#[tokio::test]
async fn a_bound_but_silent_socket_is_probed_not_respawned() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();

    let err = connect_or_spawn_with(
        &path,
        || unreachable!("something is already bound here"),
        fast_opts(),
    )
    .await
    .unwrap_err();

    assert!(matches!(err, SpawnError::DeadlineExpired { .. }));
}
