//! `kill`: shuts the shepherd down.
//!
//! `kill` sends `Request::KillDaemon` and expects `Response::ShuttingDown`,
//! then — per the wire sequence — the connection closes while the daemon
//! finishes tearing itself down. A reply alone is not success: [`kill`]
//! polls for the socket file to actually disappear before reporting one, so
//! `shep kill && shep start` cannot race the old daemon's own unlink.
//!
//! **And a second path, for when the socket is itself the problem.** That
//! request rides over a connection the client only has after a handshake, so
//! a daemon that refuses the handshake — on protocol skew, say — could not
//! be stopped by this verb, and no other verb in shep could stop it either:
//! a live daemon, a live flock, and no way forward. So a connect failure of
//! any kind falls through to [`kill_socket_free`], which proves the recorded
//! pid through the pidfile lock and signals it directly.

use std::path::Path;
use std::time::{Duration, Instant};

use shep_client::Client;
use shep_core::paths::ShepPaths;
use shep_core::protocol::{Request, Response};
use shep_daemon::boot::{self, Shepherd};

use crate::exit::ExitCode;
use crate::output::{KillRow, Streams, emit, write_outcome};

/// How long `kill` waits for the socket file to disappear after the daemon
/// acknowledges shutdown. `RunningDaemon::run` unlinks it as its last step
/// (`boot.rs:727`), behind the full kill ladder over every online sheep
/// (`:722`) — this has to cover that ladder's whole budget, not just a
/// round trip (IR-26: named, not a prose "a few seconds").
const KILL_TEARDOWN_WAIT: Duration = Duration::from_secs(10);

/// Gap between socket-existence checks while waiting out teardown. Fixed, not
/// a backoff: the wait is already bounded and short, and a backoff would only
/// delay the common case where teardown finishes in milliseconds.
///
/// Slept with `tokio::time::sleep(..).await`, never `std::thread::sleep`.
/// `#[tokio::test]` is a current-thread runtime and the fake's delayed unlink
/// is a task on that same runtime, so a blocking sleep here parks the one
/// thread that would ever run it: the socket never disappears, the poll never
/// observes what it is waiting for, and the first test hangs to the deadline
/// with no assertion — a killed CI job instead of a failure. Same rule as
/// Global Constraints' bounded-receive line, one tier down.
const KILL_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Shuts the shepherd down, over the socket if it can and without it if it
/// cannot.
///
/// Connects here rather than being handed a [`Client`], unlike every other
/// verb: a failure to connect is not a reason for THIS verb to give up. The
/// socket-free path below is the whole point of the verb in the state that
/// motivated it — a daemon that answers and then refuses the handshake — and
/// it reports its own diagnosis, including when there is genuinely nothing
/// running.
///
/// See [`kill_with_wait`] for the socket path's contract and
/// [`kill_socket_free`] for the other one.
pub async fn kill(paths: &ShepPaths, streams: &mut Streams<'_>) -> ExitCode {
    match Client::connect(&paths.socket).await {
        Ok(client) => kill_with_wait(client, streams, KILL_TEARDOWN_WAIT).await,
        // Every connect failure lands here, deliberately: a refused
        // handshake, a socket nothing is listening on, and a home with no
        // socket at all are one question from this verb's point of view —
        // does a live shepherd own this home — and the lock answers it
        // better than the connect error does.
        Err(_) => kill_socket_free(paths, streams).await,
    }
}

/// Stops a shepherd without the socket, for when the socket is the problem.
///
/// Proves the pid through [`boot::daemon_liveness`] before signalling. A
/// stale pidfile still names a pid, and that pid may since have been reused
/// by something unrelated, so the file alone is never evidence: a live
/// shepherd HOLDS the pidfile lock and the kernel drops it on process death,
/// which is the one claim a leftover file cannot make.
///
/// `SIGTERM`, never `SIGKILL`. The daemon's own handler drives the graceful
/// teardown that runs the kill ladder over every online sheep before
/// stopping, so the flock stops cleanly instead of being orphaned.
///
/// Reports, and exits non-zero, when no live shepherd owns this home, when
/// one is still starting and has no pid to signal yet, and on Windows, which
/// has no signal to send.
pub async fn kill_socket_free(paths: &ShepPaths, streams: &mut Streams<'_>) -> ExitCode {
    kill_socket_free_with_wait(paths, streams, KILL_TEARDOWN_WAIT).await
}

/// As [`kill_socket_free`], but with a caller-chosen teardown wait — the
/// same injectable-timing shape [`kill_with_wait`] carries, and for the same
/// reason.
async fn kill_socket_free_with_wait(
    paths: &ShepPaths,
    streams: &mut Streams<'_>,
    wait: Duration,
) -> ExitCode {
    let pid = match boot::daemon_liveness(paths) {
        Ok(Shepherd::Running(pid)) => pid,
        // Alive and owns the home, but between taking the lock and
        // recording its pid there is nothing to signal. Not an absence, and
        // not a pid to guess at: the honest answer is to say so and let the
        // operator ask again in a moment.
        Ok(Shepherd::Booting) => {
            let message = "a shepherd is starting up and has not recorded its pid yet; try again";
            return streams.fail(ExitCode::DaemonUnreachable, message);
        }
        Ok(Shepherd::Absent) => {
            let message = format!(
                "no shepherd is running (nothing holds the lock on `{}`)",
                boot::pidfile(paths).display()
            );
            return streams.fail(ExitCode::DaemonUnreachable, &message);
        }
        Err(err) => return streams.fail(ExitCode::Failure, &err.to_string()),
    };

    // Two arms for the same reason `control_address_answers` has two: the
    // platforms do not offer the same thing. Unix has a signal whose handler
    // the daemon already installs; Windows has no way to deliver an
    // arbitrary console control event to another process, so there is
    // nothing here to write and guessing at one would stop the shepherd
    // without walking the ladder.
    #[cfg(unix)]
    {
        use nix::sys::signal::{self, Signal};
        use nix::unistd::Pid;

        let Ok(target) = i32::try_from(pid) else {
            let message = format!("the recorded pid {pid} is not one this platform can signal");
            return streams.fail(ExitCode::Internal, &message);
        };
        // SIGTERM, not SIGKILL: the daemon's own handler runs the kill
        // ladder over every online sheep before it exits, so the flock stops
        // cleanly rather than being orphaned with broken pipes.
        if let Err(errno) = signal::kill(Pid::from_raw(target), Signal::SIGTERM) {
            let message = format!("could not signal the shepherd at pid {pid}: {errno}");
            return streams.fail(ExitCode::Failure, &message);
        }
        // The same completion check the socket path uses, so a socket-free
        // stop is no more willing to claim success before teardown finishes
        // than a socket one is.
        if wait_for_socket_to_disappear(&paths.socket, wait).await {
            write_outcome(emit(
                &mut *streams.out,
                streams.fmt,
                "kill",
                KillRow {
                    pid,
                    socket_removed: true,
                },
                streams.style,
            ))
        } else {
            let message = "the shepherd was signalled, but teardown is still in progress";
            streams.fail(ExitCode::DeadlineExceeded, message)
        }
    }
    #[cfg(windows)]
    {
        let _ = wait;
        let message = format!(
            "stopping the shepherd without the control pipe is not available on Windows: \
             there is no signal to send it. The shepherd (pid {pid}) does handle the console \
             control events, so press Ctrl-C in the window it is running in, or close that \
             window, and it will stop its flock on the way out"
        );
        streams.fail(ExitCode::Failure, &message)
    }
}

/// As [`kill`], but with a caller-chosen teardown wait — the same
/// injectable-timing shape `commands::lifecycle`'s `start` uses for
/// `START_DEADLINE`, here so the timeout branch can be proven in
/// milliseconds rather than the real ten seconds.
///
/// Takes `client` **by value** and drops it right after reading the reply:
/// the daemon closes this connection as it tears down, so holding onto the
/// `Client` would only earn a `RequestError::Closed` on the way out that the
/// caller would have to learn to ignore. The socket path and pid are copied
/// out first, since `HelloAck` carries neither a path (`Client::socket`'s
/// own doc explains why) — only `daemon_version`, `protocol` and `pid`.
///
/// A reply of `Response::ShuttingDown` is not success by itself: this then
/// polls `wait`, at [`KILL_POLL_INTERVAL`], for the socket file to actually
/// disappear, so `shep kill && shep start` cannot race the old daemon's own
/// unlink. If `wait` elapses first, this reports that teardown is still in
/// progress rather than claiming a clean stop.
///
/// A new daemon binding the same path mid-poll could in principle make the
/// file reappear and let this loop observe it and hang on. This is
/// deliberately undefended: nothing starts a daemon between the two
/// statements above, and the loser of any such race exits 10, so there is
/// nothing here for a defense to protect against.
pub async fn kill_with_wait(client: Client, streams: &mut Streams<'_>, wait: Duration) -> ExitCode {
    let socket = client.socket().to_path_buf();
    let pid = client.daemon().pid;

    let response = client.request(Request::KillDaemon).await;
    drop(client);

    match response {
        Ok(Response::ShuttingDown) => {
            if wait_for_socket_to_disappear(&socket, wait).await {
                write_outcome(emit(
                    &mut *streams.out,
                    streams.fmt,
                    "kill",
                    KillRow {
                        pid,
                        socket_removed: true,
                    },
                    streams.style,
                ))
            } else {
                let message = "the daemon acknowledged shutdown, but teardown is still in progress";
                streams.fail(ExitCode::DeadlineExceeded, message)
            }
        }
        Ok(_) => {
            let message = "the daemon answered with a response this client does not understand";
            streams.fail(ExitCode::Internal, message)
        }
        Err(err) => {
            let code = ExitCode::from(&err);
            streams.fail(code, &err.to_string())
        }
    }
}

/// Polls for the control address to stop answering, checking every
/// [`KILL_POLL_INTERVAL`], up to `wait`. Returns whether it actually went
/// away within budget.
///
/// **The two platforms ask a different question here, because the address is
/// a different kind of thing.** On unix it is a socket FILE, and the daemon
/// unlinks it on its way out, so its absence is the proof that the shutdown
/// finished. On Windows it is a named pipe: there is no directory entry to
/// watch, and `Path::exists` on `\\.\pipe\...` answers about a filesystem
/// that has no such path — it would return `false` immediately and report a
/// shutdown as complete the instant it was requested, which is exactly the
/// false success `shep kill`'s own contract exists to prevent.
///
/// So the Windows arm probes the pipe instead. A pipe stops existing when
/// its last handle closes, which happens when the daemon exits, so a connect
/// that fails with `ERROR_FILE_NOT_FOUND` is the same evidence the missing
/// socket file gives on unix. `ERROR_PIPE_BUSY` is deliberately treated as
/// STILL ALIVE rather than gone: it means the pipe is there and every
/// instance is in use, which is a daemon that has not finished.
async fn wait_for_socket_to_disappear(socket: &Path, wait: Duration) -> bool {
    let start = Instant::now();
    loop {
        if !control_address_answers(socket) {
            return true;
        }
        if start.elapsed() >= wait {
            return false;
        }
        tokio::time::sleep(KILL_POLL_INTERVAL).await;
    }
}

/// Whether the control address is still there — see
/// [`wait_for_socket_to_disappear`] for why this is a file check on one
/// platform and a connect probe on the other.
fn control_address_answers(socket: &Path) -> bool {
    #[cfg(unix)]
    {
        socket.exists()
    }
    #[cfg(windows)]
    {
        /// `ERROR_FILE_NOT_FOUND` — the pipe name no longer exists, which is
        /// what a departed daemon leaves behind.
        const ERROR_FILE_NOT_FOUND: i32 = 2;
        match std::fs::OpenOptions::new().read(true).open(socket) {
            // Connected: something is still serving.
            Ok(_) => true,
            Err(err) if err.raw_os_error() == Some(ERROR_FILE_NOT_FOUND) => false,
            // Anything else (busy, access denied) means the pipe is still
            // present. Fail towards "still alive" so `shep kill` reports a
            // timeout rather than a shutdown that did not happen.
            Err(_) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use shep_client::testing::fake_client_on;

    use super::*;
    use crate::cli::Format;
    use crate::exit::ExitCode;
    use crate::output::Streams;

    /// A [`ShepPaths`] rooted at `dir`, with the two directories the
    /// socket-free path reads under it: `pids/` for the pidfile and `run/`
    /// for the control socket. `shep_daemon::boot::init_dirs` is
    /// `pub(crate)` over there, so this creates them rather than calling it.
    ///
    /// One tempdir per test, never shared (IR-33/34).
    fn test_paths(dir: &tempfile::TempDir) -> ShepPaths {
        let paths = ShepPaths::resolve(
            &|key| (key == "SHEP_HOME").then(|| dir.path().to_string_lossy().into_owned()),
            Path::new("/nonexistent"),
        );
        std::fs::create_dir_all(&paths.pids).unwrap();
        std::fs::create_dir_all(&paths.run).unwrap();
        paths
    }

    /// Holds `paths`'s pidfile lock the way a live shepherd does — an
    /// `flock` on the pidfile itself — and records `pid` in it, or leaves
    /// the file empty to stand in for a shepherd that has not reached its
    /// own `record` yet.
    ///
    /// `flock` conflicts between open file DESCRIPTIONS rather than between
    /// processes, so the lock this returns contends with `daemon_liveness`'s
    /// own acquire even though both run inside this one test binary.
    #[cfg(unix)]
    fn hold_pidfile_lock(paths: &ShepPaths, pid: Option<u32>) -> nix::fcntl::Flock<std::fs::File> {
        use std::io::Write as _;

        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(boot::pidfile(paths))
            .unwrap();
        let mut lock =
            nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock).unwrap();
        if let Some(pid) = pid {
            write!(&mut *lock, "{pid}").unwrap();
            lock.flush().unwrap();
        }
        lock
    }

    /// The incident case: a daemon that answers the socket and then refuses
    /// at the handshake. Every verb reaches the shepherd through that
    /// handshake, `kill` included, so before this fallback a version-skewed
    /// daemon could not be stopped by anything in shep.
    ///
    /// `cfg(unix)` for the FAKE's mechanism, not the feature's — the same
    /// distinction the note on
    /// `a_teardown_that_never_finishes_reports_in_progress_not_success`
    /// draws. The stand-in shepherd is a real child process holding a real
    /// pid, and the proof it was stopped gracefully is the signal number in
    /// its exit status.
    #[cfg(unix)]
    #[tokio::test]
    async fn kill_falls_back_to_the_pidfile_when_the_handshake_refuses() {
        use std::os::unix::process::ExitStatusExt as _;

        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let refusal = Err(shep_core::protocol::RpcError {
            code: shep_core::protocol::RpcErrorCode::ProtocolMismatch,
            message: "daemon speaks protocol 1, client sent 2".to_string(),
        });
        let _daemon = shep_client::testing::fake_daemon(&paths.socket, refusal).await;

        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let _lock = hold_pidfile_lock(&paths, Some(child.id()));

        // Stands in for the daemon's own last teardown step: a real
        // shepherd unlinks the socket as it exits, and that unlink is what
        // `wait_for_socket_to_disappear` waits for. On its own thread
        // because `Child::wait` blocks, and the poll loop it has to make
        // progress against is a task on this test's single-threaded runtime.
        let socket = paths.socket.clone();
        let reaper = std::thread::spawn(move || {
            let mut child = child;
            let status = child.wait().unwrap();
            let _ = std::fs::remove_file(&socket);
            status
        });

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            kill(&paths, &mut streams).await
        };
        assert_eq!(code, ExitCode::Success, "{}", String::from_utf8_lossy(&err));
        let status = reaper.join().unwrap();
        assert_eq!(
            status.signal(),
            Some(nix::sys::signal::Signal::SIGTERM as i32),
            "the flock stops cleanly only if the daemon got its own handler's signal"
        );
    }

    #[tokio::test]
    async fn kill_refuses_a_pid_the_lock_does_not_prove_is_sheps() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        std::fs::write(boot::pidfile(&paths), "999999").unwrap();

        // Stale file, nothing holds the lock. Signalling 999999 could hit an
        // unrelated process that has since been given that pid, so this must
        // refuse rather than guess.
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            kill(&paths, &mut streams).await
        };
        assert_ne!(code, ExitCode::Success);
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("no shepherd"), "{err}");
    }

    /// A shepherd between `PidfileLock::acquire` and its own `record` holds
    /// the lock with nothing written in the file. It is alive and owns the
    /// home, so reporting it as an absence would send an operator on to
    /// start a second daemon that then dies unable to take the lock.
    ///
    /// `cfg(unix)` for the lock helper's mechanism, as above.
    #[cfg(unix)]
    #[tokio::test]
    async fn kill_reports_a_booting_shepherd_rather_than_an_absence() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let _lock = hold_pidfile_lock(&paths, None);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            kill(&paths, &mut streams).await
        };
        assert_ne!(code, ExitCode::Success);
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("starting up"), "{err}");
        assert!(
            !err.contains("no shepherd"),
            "a shepherd that is starting is not an absent one: {err}"
        );
    }

    #[tokio::test]
    async fn kill_waits_for_the_socket_to_disappear_before_reporting_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        // A fake daemon that replies ShuttingDown, waits, THEN unlinks.
        let (client, daemon) = fake_client_on(&path).await;
        daemon.reply_shutting_down_then_unlink_after(Duration::from_millis(120));

        assert!(path.exists());
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            kill_with_wait(client, &mut streams, KILL_TEARDOWN_WAIT).await
        };
        assert_eq!(code, ExitCode::Success);
        assert!(
            !path.exists(),
            "success must mean the socket is actually gone"
        );
    }

    /// `cfg(unix)` because the FAKE's mechanism is unix-only, not because
    /// the behaviour is. `reply_shutting_down_and_never_unlink` simulates a
    /// wedged teardown by declining to unlink a socket FILE — and a named
    /// pipe has no file to decline to unlink. The pipe stops existing when
    /// its owner's last handle closes, so a fake that is still running
    /// cannot represent "the shepherd said it was going and then did not
    /// go".
    ///
    /// The production code this guards DOES have a Windows arm:
    /// `control_address_answers` probes the pipe rather than stat-ing a
    /// path, and deliberately reads `ERROR_PIPE_BUSY` as still-alive so a
    /// wedged daemon times out instead of reporting success. That arm was
    /// exercised against a real Windows shepherd (`shep kill` reported
    /// `SOCKET_REMOVED true` only once the daemon had actually exited);
    /// what is missing here is a fake that can reproduce it, not the code.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_teardown_that_never_finishes_reports_in_progress_not_success() {
        // Fake daemon replies ShuttingDown and never unlinks. Uses an injected
        // short wait, not KILL_TEARDOWN_WAIT — the test proves the branch, not
        // that ten seconds elapse.
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, daemon) = fake_client_on(&path).await;
        daemon.reply_shutting_down_and_never_unlink();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            kill_with_wait(client, &mut streams, Duration::from_millis(80)).await
        };
        assert_eq!(code, ExitCode::DeadlineExceeded);
        assert!(
            path.exists(),
            "precondition: the fake really did leave the socket behind"
        );
    }
}
