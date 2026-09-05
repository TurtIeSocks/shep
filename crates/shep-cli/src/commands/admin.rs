//! `kill`: shuts the shepherd down.
//!
//! A `Response::ShuttingDown` reply is not success: [`kill`] polls for the
//! control address to stop answering first, so `shep kill && shep start`
//! cannot race the old daemon's unlink. Any connect failure falls through to
//! [`kill_socket_free`], which proves the recorded pid through the pidfile
//! lock and signals it directly.

use std::path::Path;
use std::time::{Duration, Instant};

use shep_client::Client;
use shep_core::paths::ShepPaths;
use shep_core::protocol::{Request, Response};
use shep_daemon::boot::{self, Shepherd};

use crate::exit::ExitCode;
use crate::output::{KillRow, Streams, emit, write_outcome};

/// How long `kill` waits for the control address to stop answering after the
/// daemon acknowledges shutdown. Covers the whole kill ladder over every
/// online sheep, which `RunningDaemon::run` runs before it unlinks.
///
/// `pub(crate)`: `commands::daemon`'s reload waits out the same teardown.
pub(crate) const KILL_TEARDOWN_WAIT: Duration = Duration::from_secs(10);

/// Gap between socket-existence checks while waiting out teardown.
///
/// Must be slept on asynchronously: a current-thread runtime parks the one
/// thread that would run the unlink this loop is waiting for.
const KILL_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Shuts the shepherd down, over the socket if it can and without it if it
/// cannot.
///
/// Connects itself, since a failure to connect is not a reason for this verb
/// to give up. [`kill_socket_free`] reports its own diagnosis, including when
/// nothing is running.
pub async fn kill(paths: &ShepPaths, streams: &mut Streams<'_>) -> ExitCode {
    match Client::connect(&paths.socket).await {
        Ok(client) => kill_with_wait(client, streams, KILL_TEARDOWN_WAIT).await,
        // A refused handshake, a dead socket and no socket at all are one
        // question: does a live shepherd own this home. The pidfile lock
        // answers it.
        Err(_) => kill_socket_free(paths, streams).await,
    }
}

/// Stops a shepherd without the socket, for when the socket is the problem.
///
/// Proves the pid through [`boot::daemon_liveness`] before signalling: a
/// stale pidfile names a pid that may since have been reused, and only a live
/// shepherd holds the lock.
///
/// Exits non-zero when no live shepherd owns this home, when one is still
/// starting and has no pid to signal yet, and on Windows.
pub async fn kill_socket_free(paths: &ShepPaths, streams: &mut Streams<'_>) -> ExitCode {
    kill_socket_free_with_wait(paths, streams, KILL_TEARDOWN_WAIT).await
}

/// As [`kill_socket_free`], with a caller-chosen teardown wait.
async fn kill_socket_free_with_wait(
    paths: &ShepPaths,
    streams: &mut Streams<'_>,
    wait: Duration,
) -> ExitCode {
    let pid = match boot::daemon_liveness(paths) {
        Ok(Shepherd::Running(pid)) => pid,
        // Alive and owns the home, but has not recorded a pid yet. Not an
        // absence, and not a pid to guess at.
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

    if let Err((code, message)) = signal_graceful_stop(pid) {
        return streams.fail(code, &message);
    }
    #[cfg(unix)]
    {
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
    // Unreachable: `signal_graceful_stop` already returned on this platform.
    #[cfg(windows)]
    {
        let _ = wait;
        ExitCode::Failure
    }
}

/// Asks the shepherd at `pid` to stop the way its own handler does.
///
/// Shared by `kill`'s fallback and `commands::daemon`'s reload, so both stop
/// a shepherd the same way per platform. `SIGTERM`, never `SIGKILL`: the
/// daemon's own handler runs the kill ladder over every online sheep before
/// stopping. Windows cannot deliver a console control event to another
/// process, so there is nothing to send.
///
/// # Errors
/// The exit code and the sentence to report, when the pid is not one this
/// platform can name, when the signal failed, or on Windows.
pub(crate) fn signal_graceful_stop(pid: u32) -> Result<(), (ExitCode, String)> {
    #[cfg(unix)]
    {
        use nix::sys::signal::{self, Signal};
        use nix::unistd::Pid;

        let Ok(target) = i32::try_from(pid) else {
            let message = format!("the recorded pid {pid} is not one this platform can signal");
            return Err((ExitCode::Internal, message));
        };
        signal::kill(Pid::from_raw(target), Signal::SIGTERM).map_err(|errno| {
            let message = format!("could not signal the shepherd at pid {pid}: {errno}");
            (ExitCode::Failure, message)
        })
    }
    #[cfg(windows)]
    {
        let message = format!(
            "stopping the shepherd without the control pipe is not available on Windows: \
             there is no signal to send it. The shepherd (pid {pid}) does handle the console \
             control events, so press Ctrl-C in the window it is running in, or close that \
             window, and it will stop its flock on the way out"
        );
        Err((ExitCode::Failure, message))
    }
}

/// As [`kill`], with a caller-chosen teardown wait.
///
/// Takes `client` by value and drops it after reading the reply: the daemon
/// closes this connection as it tears down.
///
/// Polls up to `wait` for the control address to go away, and reports
/// teardown still in progress if it elapses first.
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

/// Polls the control address every [`KILL_POLL_INTERVAL`] for up to `wait`.
/// Returns whether it stopped answering within budget.
///
/// `pub(crate)`: `commands::daemon`'s reload waits out the same teardown
/// before starting a successor, and should reuse this rather than poll again.
pub(crate) async fn wait_for_socket_to_disappear(socket: &Path, wait: Duration) -> bool {
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

/// Whether the control address is still there.
///
/// On unix the daemon unlinks the socket file on its way out, so its absence
/// proves teardown finished. A named pipe has no directory entry, so the
/// Windows arm probes the pipe: `ERROR_FILE_NOT_FOUND` is the same evidence.
fn control_address_answers(socket: &Path) -> bool {
    #[cfg(unix)]
    {
        socket.exists()
    }
    #[cfg(windows)]
    {
        const ERROR_FILE_NOT_FOUND: i32 = 2;
        match std::fs::OpenOptions::new().read(true).open(socket) {
            Ok(_) => true,
            Err(err) if err.raw_os_error() == Some(ERROR_FILE_NOT_FOUND) => false,
            // Busy or access denied: still present. Fail towards alive, so
            // `kill` reports a timeout rather than a shutdown that did not
            // happen.
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

    /// A [`ShepPaths`] rooted at `dir`, with `pids/` and `run/` created.
    fn test_paths(dir: &tempfile::TempDir) -> ShepPaths {
        let paths = ShepPaths::resolve(
            &|key| (key == "SHEP_HOME").then(|| dir.path().to_string_lossy().into_owned()),
            Path::new("/nonexistent"),
        );
        std::fs::create_dir_all(&paths.pids).unwrap();
        std::fs::create_dir_all(&paths.run).unwrap();
        paths
    }

    /// Holds `paths`'s pidfile lock the way a live shepherd does, recording
    /// `pid` or leaving the file empty for one that has not reached its own
    /// `record` yet.
    ///
    /// `flock` conflicts between open file descriptions, so this contends
    /// with `daemon_liveness`'s own acquire inside one test binary.
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

    /// `cfg(unix)` for the fake: the stand-in shepherd is a real child, and
    /// the proof it stopped gracefully is the signal in its exit status.
    #[cfg(unix)]
    #[tokio::test]
    async fn kill_falls_back_to_the_pidfile_when_the_handshake_refuses() {
        use std::os::unix::process::ExitStatusExt as _;

        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let refusal = Err(shep_core::protocol::RpcError {
            code: shep_core::protocol::RpcErrorCode::ProtocolMismatch,
            message: "daemon speaks protocol 1, client sent 2".to_string(),
            daemon_version: None,
        });
        let _daemon = shep_client::testing::fake_daemon(&paths.socket, refusal).await;

        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let _lock = hold_pidfile_lock(&paths, Some(child.id()));

        // Stands in for the daemon's last teardown step, the unlink. On its
        // own thread because `Child::wait` blocks and the poll loop is a task
        // on this test's single-threaded runtime.
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
        // unrelated process that has since been given that pid.
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
    /// the lock with nothing written in the file.
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

    /// `cfg(unix)` for the fake: it wedges teardown by declining to unlink a
    /// socket file, and a named pipe has no file to decline to unlink.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_teardown_that_never_finishes_reports_in_progress_not_success() {
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
