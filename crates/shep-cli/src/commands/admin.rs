//! `kill`: shuts the shepherd down.
//!
//! `kill` sends `Request::KillDaemon` and expects `Response::ShuttingDown`,
//! then — per the wire sequence — the connection closes while the daemon
//! finishes tearing itself down. A reply alone is not success: [`kill`]
//! polls for the socket file to actually disappear before reporting one, so
//! `shep kill && shep start` cannot race the old daemon's own unlink.

use std::path::Path;
use std::time::{Duration, Instant};

use shep_client::Client;
use shep_core::protocol::{Request, Response};

use crate::cli::Format;
use crate::exit::ExitCode;
use crate::output::{KillRow, Streams, emit, emit_error, write_outcome};

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

/// Shuts the shepherd down.
///
/// Delegates to [`kill_with_wait`] with [`KILL_TEARDOWN_WAIT`]. Production's
/// only call site; see that function's own doc for the full contract.
pub async fn kill(client: Client, streams: &mut Streams<'_>, fmt: Format) -> ExitCode {
    kill_with_wait(client, streams, fmt, KILL_TEARDOWN_WAIT).await
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
pub async fn kill_with_wait(
    client: Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    wait: Duration,
) -> ExitCode {
    let socket = client.socket().to_path_buf();
    let pid = client.daemon().pid;

    let response = client.request(Request::KillDaemon).await;
    drop(client);

    match response {
        Ok(Response::ShuttingDown) => {
            if wait_for_socket_to_disappear(&socket, wait).await {
                write_outcome(emit(
                    &mut *streams.out,
                    fmt,
                    "kill",
                    KillRow {
                        pid,
                        socket_removed: true,
                    },
                ))
            } else {
                let message =
                    "the shepherd acknowledged shutdown, but teardown is still in progress";
                let _ = emit_error(
                    &mut *streams.err,
                    fmt,
                    ExitCode::DeadlineExceeded.code_str(),
                    message,
                );
                ExitCode::DeadlineExceeded
            }
        }
        Ok(_) => {
            let message = "the daemon answered with a response this client does not understand";
            let _ = emit_error(
                &mut *streams.err,
                fmt,
                ExitCode::Internal.code_str(),
                message,
            );
            ExitCode::Internal
        }
        Err(err) => {
            let code = ExitCode::from(&err);
            let _ = emit_error(&mut *streams.err, fmt, code.code_str(), &err.to_string());
            code
        }
    }
}

/// Polls for `socket` to disappear, checking every [`KILL_POLL_INTERVAL`],
/// up to `wait`. Returns whether it actually disappeared within budget.
async fn wait_for_socket_to_disappear(socket: &Path, wait: Duration) -> bool {
    let start = Instant::now();
    loop {
        if !socket.exists() {
            return true;
        }
        if start.elapsed() >= wait {
            return false;
        }
        tokio::time::sleep(KILL_POLL_INTERVAL).await;
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

    #[tokio::test]
    async fn kill_waits_for_the_socket_to_disappear_before_reporting_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
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
            };
            kill(client, &mut streams, Format::Table).await
        };
        assert_eq!(code, ExitCode::Success);
        assert!(
            !path.exists(),
            "success must mean the socket is actually gone"
        );
    }

    #[tokio::test]
    async fn a_teardown_that_never_finishes_reports_in_progress_not_success() {
        // Fake daemon replies ShuttingDown and never unlinks. Uses an injected
        // short wait, not KILL_TEARDOWN_WAIT — the test proves the branch, not
        // that ten seconds elapse.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, daemon) = fake_client_on(&path).await;
        daemon.reply_shutting_down_and_never_unlink();

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            kill_with_wait(
                client,
                &mut streams,
                Format::Table,
                Duration::from_millis(80),
            )
            .await
        };
        assert_eq!(code, ExitCode::DeadlineExceeded);
        assert!(
            path.exists(),
            "precondition: the fake really did leave the socket behind"
        );
    }
}
