//! [`OsProber`]: HTTP, TCP and exec probes over real sockets and processes.
//!
//! Hand-rolled over `tokio::net::TcpStream`, not a client crate. No TLS
//! (`https://` is rejected at config time) and no redirects: a `301` is a
//! [`ProbeFailure::Rejected`], never followed, since a probe that follows
//! redirects can pass against a different service. For the same reason a
//! response counts as healthy only if its status line begins `HTTP/`.

use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::time::Duration;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;

use shep_core::config::ProbeTarget;

use super::{ProbeFailure, Prober};

/// Longest status line `OsProber` will read before giving up on a response.
///
/// An HTTP/1.1 status line is tens of bytes. This bound exists so a probe
/// target that is not an HTTP server cannot stream unbounded data into the
/// daemon's heap.
const HTTP_STATUS_LINE_CAP: u64 = 8 * 1024;

/// The port RFC 7230 §5.4 lets a `Host:` header leave out. `ProbeTarget`
/// already defaults a portless `http://` target to this, so a header built
/// from one carries no port either way.
const HTTP_DEFAULT_PORT: u16 = 80;

/// `Prober` over real sockets and real processes.
pub struct OsProber {
    /// Working directory for exec probes; `None` inherits the daemon's own.
    cwd: Option<PathBuf>,
    /// Environment for exec probes, usually the same `PORT` the sheep was
    /// given. `Debug` does not leak these values.
    env: BTreeMap<String, String>,
}

impl OsProber {
    /// A prober that runs exec probes in `cwd` with `env`.
    #[must_use]
    pub fn new(cwd: Option<PathBuf>, env: BTreeMap<String, String>) -> Self {
        Self { cwd, env }
    }
}

/// Redacting: env may carry secrets like `DATABASE_URL`.
///
/// `finish_non_exhaustive`, not `finish`: a redacting impl should not claim
/// it printed every field there is.
impl fmt::Debug for OsProber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OsProber")
            .field("cwd", &self.cwd)
            .field("env", &format_args!("<{} vars>", self.env.len()))
            .finish_non_exhaustive()
    }
}

impl Prober for OsProber {
    fn probe<'a>(
        &'a self,
        target: &'a ProbeTarget,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProbeFailure>> + Send + 'a>> {
        // `ProbeTarget` is not `#[non_exhaustive]`, so a fourth transport
        // fails `cargo check` here rather than silently falling through a
        // `_` arm.
        Box::pin(async move {
            match target {
                ProbeTarget::Http { host, port, path } => {
                    probe_http(host, *port, path, timeout).await
                }
                ProbeTarget::Tcp { host, port } => probe_tcp(host, *port, timeout).await,
                ProbeTarget::Exec { command } => self.probe_exec(command, timeout).await,
            }
        })
    }
}

impl OsProber {
    /// Runs `command` through the platform shell, giving up after `timeout`.
    async fn probe_exec(&self, command: &str, timeout: Duration) -> Result<(), ProbeFailure> {
        #[cfg(unix)]
        let mut cmd = {
            let mut c = Command::new("sh");
            c.arg("-c").arg(command);
            c
        };
        #[cfg(windows)]
        let mut cmd = {
            let mut c = Command::new("cmd");
            c.arg("/C").arg(command);
            c
        };

        // Kills and reaps the shell when `child` goes out of scope, so a
        // timed-out probe leaves no zombie. Only reaches the shell itself;
        // `kill_probe_group` below reaches what it forked.
        cmd.kill_on_drop(true);
        // Puts the shell in a process group of its own, whose pgid is its
        // own pid: without it `-pid` names no group and the timeout path
        // has nothing group-wide to signal.
        #[cfg(unix)]
        cmd.process_group(0);
        // A probe's output is the probe's business, never the daemon's: the
        // default is inheritance, which would put a `curl`-style probe's
        // response body into the daemon's own stdout once per interval,
        // forever.
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        // The probe sees the sheep's environment, never the daemon's,
        // matching `SpawnSpec::env`'s rule for a sheep.
        cmd.env_clear().envs(&self.env);

        let mut child = cmd
            .spawn()
            .map_err(|err| ProbeFailure::Transport(err.to_string()))?;
        // Bound to a `let`, not matched inline: a `match` would keep
        // `child.wait()`'s `&mut child` borrow alive across every arm, and
        // the abandon arms need `child` back to signal its group.
        let waited = tokio::time::timeout(timeout, child.wait()).await;
        match waited {
            Ok(Ok(status)) if status.success() => Ok(()),
            // A command naming a nonexistent binary is `Rejected("127")`,
            // not a spawn failure: `sh` itself always spawns and reports
            // "not found" through its own exit code. Exec straight to the
            // program instead and the same 127 becomes a real spawn failure.
            Ok(Ok(status)) => Err(ProbeFailure::Rejected(exit_code_text(&status))),
            // Both remaining arms abandon a process that may still be
            // running: the timeout by definition, and a `wait` error (e.g.
            // ECHILD) without ever confirming the child is gone.
            Ok(Err(err)) => {
                kill_probe_group(&child);
                Err(ProbeFailure::Transport(err.to_string()))
            }
            Err(_elapsed) => {
                kill_probe_group(&child);
                Err(ProbeFailure::Timeout)
            }
        }
    }
}

/// SIGKILLs the whole process group of a probe child being abandoned.
///
/// Reuses the runner's own [`signal_group`](crate::tokio_runner::signal_group):
/// `sh -c 'thing & …'` leaves a fork that a leader-only kill never reaches,
/// the same orphan shape the stop rungs handle for a sheep.
///
/// Failure is logged, never returned: the verdict a timed-out probe reports
/// is [`ProbeFailure::Timeout`] either way, and `ESRCH` here just means the
/// group was already gone.
#[cfg(unix)]
fn kill_probe_group(child: &tokio::process::Child) {
    // `None` once the child has been waited to completion: nothing left to
    // signal, and a pid the OS may have recycled since must never get sent SIGKILL.
    let Some(pid) = child.id() else {
        return;
    };
    if let Err(error) = crate::tokio_runner::signal_group(pid, nix::sys::signal::Signal::SIGKILL) {
        tracing::warn!(pid, %error, "probe process group kill failed");
    }
}

/// Windows has no process group this can signal, and `kill_on_drop` reaches
/// the `cmd` alone. `TokioRunner` and its own group signalling are
/// `#[cfg(unix)]`, so a Windows daemon has no supervised processes to leak
/// in the first place.
#[cfg(windows)]
fn kill_probe_group(_child: &tokio::process::Child) {}

/// Renders an `ExitStatus` for [`ProbeFailure::Rejected`]. `code()` is
/// `None` on unix when the child died from a signal rather than exiting,
/// carried distinctly rather than defaulted to a number no real exit code
/// produces.
fn exit_code_text(status: &std::process::ExitStatus) -> String {
    status.code().map_or_else(
        || "terminated by signal".to_string(),
        |code| code.to_string(),
    )
}

/// Probes an HTTP target: connect, write one `GET`, read the status line,
/// pass on `200..=299`.
///
/// Connect, write and read are wrapped in one `tokio::time::timeout` rather
/// than one per step: three separate timeouts would add up to three times
/// the caller's budget.
async fn probe_http(
    host: &str,
    port: u16,
    path: &str,
    timeout: Duration,
) -> Result<(), ProbeFailure> {
    match tokio::time::timeout(timeout, http_roundtrip(host, port, path)).await {
        Ok(result) => result,
        Err(_elapsed) => Err(ProbeFailure::Timeout),
    }
}

async fn http_roundtrip(host: &str, port: u16, path: &str) -> Result<(), ProbeFailure> {
    // `(host, port)` tuple, not a formatted `"host:port"` string:
    // `ProbeTarget` strips brackets from a bracketed IPv6 literal, and only
    // the tuple form accepts a bracket-stripped host.
    let mut stream = TcpStream::connect((host, port))
        .await
        .map_err(|err| ProbeFailure::Transport(err.to_string()))?;

    let header_host = host_header(host, port);
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: {header_host}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|err| ProbeFailure::Transport(err.to_string()))?;

    let status_line = read_status_line(stream).await?;
    evaluate_status_line(&status_line)
}

/// Builds the RFC 7230 `Host:` header value for a target.
///
/// `ProbeTarget` strips brackets off an IPv6 literal like `[::1]` at parse
/// time so `TcpStream::connect` can take `(host, port)`; the header needs
/// them back, since `Host: ::1` reads as colon-separated fields rather
/// than one address.
///
/// The port is included unless it is the scheme default: a name-based
/// virtual host serving several ports routes on this header alone.
fn host_header(host: &str, port: u16) -> String {
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    if port == HTTP_DEFAULT_PORT {
        host
    } else {
        format!("{host}:{port}")
    }
}

/// Reads up to the first `\r\n`, or [`HTTP_STATUS_LINE_CAP`] bytes,
/// whichever comes first.
async fn read_status_line(stream: TcpStream) -> Result<String, ProbeFailure> {
    let mut reader = BufReader::new(stream.take(HTTP_STATUS_LINE_CAP));
    let mut buf = Vec::new();
    reader
        .read_until(b'\n', &mut buf)
        .await
        .map_err(|err| ProbeFailure::Transport(err.to_string()))?;
    if buf.is_empty() {
        return Err(ProbeFailure::Transport(
            "connection closed before a response was received".to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Parses the numeric status out of an HTTP status line (`HTTP/1.1 200 OK`
/// -> `200`) and maps it to a pass or a [`ProbeFailure::Rejected`].
///
/// Requires the `HTTP/` prefix: checking only "the second token" would
/// read `BANANA 204 whatever` from a non-HTTP service as healthy. Never
/// panics on a malformed line: `.nth(1)` and `.parse().ok()` both return
/// `None` rather than panicking.
///
/// A line with no parseable status is `Rejected`, not `Transport`: the
/// connection succeeded and bytes came back, so a verdict was possible,
/// just negative.
fn evaluate_status_line(line: &str) -> Result<(), ProbeFailure> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    let Some(after_version) = trimmed.strip_prefix("HTTP/") else {
        return Err(ProbeFailure::Rejected(format!(
            "not an HTTP status line: {trimmed:?}"
        )));
    };
    // Counted over what follows `HTTP/`, so the token taken is still the one
    // after the version: `1.1 200 OK` -> `200`.
    match after_version
        .split(' ')
        .nth(1)
        .and_then(|token| token.parse::<u16>().ok())
    {
        Some(200..=299) => Ok(()),
        Some(code) => Err(ProbeFailure::Rejected(code.to_string())),
        None => Err(ProbeFailure::Rejected(format!(
            "malformed HTTP status line: {trimmed:?}"
        ))),
    }
}

/// Probes a TCP target: pass on a successful connect, nothing more.
async fn probe_tcp(host: &str, port: u16, timeout: Duration) -> Result<(), ProbeFailure> {
    match tokio::time::timeout(timeout, TcpStream::connect((host, port))).await {
        Ok(Ok(_stream)) => Ok(()),
        Ok(Err(err)) => Err(ProbeFailure::Transport(err.to_string())),
        Err(_elapsed) => Err(ProbeFailure::Timeout),
    }
}

#[cfg(test)]
mod tests {
    // Real time, not the paused clock: these connect to real listeners or
    // spawn real child processes, and pausing `tokio::time` alone would
    // deadlock a test waiting on the kernel's own clock.

    use core::time::Duration;

    use std::collections::BTreeMap;

    use tokio::net::TcpListener;

    use super::*;
    use crate::testing::{HttpReply, loopback_http, loopback_http_on};

    /// Every test's probe timeout: generous enough that CI/loaded-machine
    /// scheduling jitter can't turn a real pass into a flaky timeout, small
    /// enough that a mistakenly-hanging probe doesn't stall the suite.
    const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

    fn http_target(port: u16, path: &str) -> ProbeTarget {
        ProbeTarget::Http {
            host: "127.0.0.1".to_string(),
            port,
            path: path.to_string(),
        }
    }

    #[tokio::test]
    async fn passing_status_codes_are_accepted_across_the_2xx_range() {
        for code in [200u16, 204, 299] {
            let server = loopback_http(vec![HttpReply::Status(code)]).await;
            let prober = OsProber::new(None, BTreeMap::new());
            let result = prober
                .probe(&http_target(server.addr.port(), "/"), PROBE_TIMEOUT)
                .await;
            assert_eq!(result, Ok(()), "status {code} should pass");
        }
    }

    #[tokio::test]
    async fn a_301_is_rejected_not_followed() {
        let server = loopback_http(vec![HttpReply::Status(301)]).await;
        let prober = OsProber::new(None, BTreeMap::new());
        let result = prober
            .probe(&http_target(server.addr.port(), "/"), PROBE_TIMEOUT)
            .await;
        assert_eq!(result, Err(ProbeFailure::Rejected("301".to_string())));
    }

    #[tokio::test]
    async fn a_500_is_rejected() {
        let server = loopback_http(vec![HttpReply::Status(500)]).await;
        let prober = OsProber::new(None, BTreeMap::new());
        let result = prober
            .probe(&http_target(server.addr.port(), "/"), PROBE_TIMEOUT)
            .await;
        assert_eq!(result, Err(ProbeFailure::Rejected("500".to_string())));
    }

    // Fails if connect/write/read each get their own `timeout`: three of
    // them would let this test take up to 3x `short`.
    #[tokio::test]
    async fn a_hanging_response_times_out_within_a_small_multiple_of_the_budget() {
        let server = loopback_http(vec![HttpReply::Hang]).await;
        let short = Duration::from_millis(150);
        let prober = OsProber::new(None, BTreeMap::new());

        let start = std::time::Instant::now();
        let result = prober
            .probe(&http_target(server.addr.port(), "/"), short)
            .await;
        let elapsed = start.elapsed();

        assert_eq!(result, Err(ProbeFailure::Timeout));
        assert!(
            elapsed < short * 3,
            "expected the probe to give up within a small multiple of {short:?}, took {elapsed:?}"
        );
    }

    // Pins the exact bytes on the wire: the fake's reply doesn't depend on
    // the request, so a prober dropping `Host:`/`Connection: close` or
    // ignoring the path would still pass every other test here.
    #[tokio::test]
    async fn an_http_probe_sends_one_get_carrying_the_targets_path_and_a_host_header() {
        let mut server = loopback_http(vec![HttpReply::Status(200)]).await;
        let prober = OsProber::new(None, BTreeMap::new());
        let result = prober
            .probe(&http_target(server.addr.port(), "/healthz"), PROBE_TIMEOUT)
            .await;
        assert_eq!(result, Ok(()));
        // The port is ephemeral, so it is formatted in rather than pinned;
        // that it appears at all is the assertion.
        let port = server.addr.port();
        assert_eq!(
            server.next_request().await,
            format!("GET /healthz HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
        );
    }

    // Isolated from the network: exercises the bracket-stripping fact
    // `ProbeTarget`'s `host` doc records, without needing real IPv6.
    #[test]
    fn an_ipv6_host_is_re_bracketed_for_the_host_header() {
        // Brackets return for IPv6, and the port rides along unless it is
        // the scheme default the header may omit.
        assert_eq!(host_header("::1", 8080), "[::1]:8080");
        assert_eq!(host_header("2001:db8::1", 443), "[2001:db8::1]:443");
        assert_eq!(host_header("::1", 80), "[::1]");
        assert_eq!(host_header("127.0.0.1", 8080), "127.0.0.1:8080");
        assert_eq!(host_header("127.0.0.1", 80), "127.0.0.1");
        assert_eq!(host_header("localhost", 9000), "localhost:9000");
    }

    // End-to-end IPv6: connect uses the bracket-stripped host, and the
    // header gets brackets back. Needs a real IPv6 loopback; a host with
    // it disabled fails clearly via `loopback_http_on`'s panic message.
    #[tokio::test]
    async fn an_ipv6_target_connects_unbracketed_and_brackets_the_host_header() {
        let mut server = loopback_http_on("[::1]:0", vec![HttpReply::Status(200)]).await;
        let target = ProbeTarget::Http {
            host: "::1".to_string(),
            port: server.addr.port(),
            path: "/".to_string(),
        };

        let prober = OsProber::new(None, BTreeMap::new());
        let result = prober.probe(&target, PROBE_TIMEOUT).await;

        assert_eq!(result, Ok(()), "an IPv6 target must connect at all");
        let port = server.addr.port();
        assert_eq!(
            server.next_request().await,
            format!("GET / HTTP/1.1\r\nHost: [::1]:{port}\r\nConnection: close\r\n\r\n")
        );
    }

    // A down service must not look like a slow one: Transport, not Timeout.
    #[tokio::test]
    async fn a_port_with_nothing_listening_fails_as_transport() {
        // Bind to grab a genuinely free port, then drop the listener so the
        // port refuses rather than being caught mid-handshake by anything
        // still listening.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let prober = OsProber::new(None, BTreeMap::new());
        let result = prober
            .probe(&http_target(addr.port(), "/"), PROBE_TIMEOUT)
            .await;
        assert!(
            matches!(result, Err(ProbeFailure::Transport(_))),
            "expected Transport, got {result:?}"
        );
    }

    // Asserts Rejected specifically, not just "does not panic": bytes came
    // back, so a verdict was possible, just negative.
    #[tokio::test]
    async fn a_garbage_first_line_is_rejected_not_panicked_on() {
        let server = loopback_http(vec![HttpReply::Raw("not http\r\n".to_string())]).await;
        let prober = OsProber::new(None, BTreeMap::new());
        let result = prober
            .probe(&http_target(server.addr.port(), "/"), PROBE_TIMEOUT)
            .await;
        assert!(
            matches!(result, Err(ProbeFailure::Rejected(_))),
            "expected Rejected, got {result:?}"
        );
    }

    // The garbage-first-line fixture has two tokens, so it only catches a
    // parser whose second token fails to parse as u16, not one that indexes
    // past the end. A well-formed `HTTP/1.1\r\n` with no second token is
    // what catches `tokens[1]` instead of `.nth(1)`.
    #[tokio::test]
    async fn a_status_line_with_no_code_after_the_version_is_rejected_not_panicked_on() {
        let server = loopback_http(vec![HttpReply::Raw("HTTP/1.1\r\n".to_string())]).await;
        let prober = OsProber::new(None, BTreeMap::new());
        let result = prober
            .probe(&http_target(server.addr.port(), "/"), PROBE_TIMEOUT)
            .await;
        assert!(
            matches!(result, Err(ProbeFailure::Rejected(_))),
            "expected Rejected, got {result:?}"
        );
    }

    // `BANANA 204 whatever` has a 2xx in the position-only slot: catches a
    // parser that never checks the `HTTP/` prefix.
    #[tokio::test]
    async fn a_2xx_from_a_service_that_is_not_http_is_rejected() {
        let server =
            loopback_http(vec![HttpReply::Raw("BANANA 204 whatever\r\n".to_string())]).await;
        let prober = OsProber::new(None, BTreeMap::new());
        let result = prober
            .probe(&http_target(server.addr.port(), "/"), PROBE_TIMEOUT)
            .await;
        assert!(
            matches!(result, Err(ProbeFailure::Rejected(_))),
            "expected Rejected, got {result:?}"
        );
    }

    #[tokio::test]
    async fn tcp_probe_against_a_bound_listener_passes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let prober = OsProber::new(None, BTreeMap::new());
        let target = ProbeTarget::Tcp {
            host: "127.0.0.1".to_string(),
            port: addr.port(),
        };
        let result = prober.probe(&target, PROBE_TIMEOUT).await;
        assert_eq!(result, Ok(()));
        drop(listener);
    }

    #[tokio::test]
    async fn tcp_probe_against_a_closed_port_fails_as_transport() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let prober = OsProber::new(None, BTreeMap::new());
        let target = ProbeTarget::Tcp {
            host: "127.0.0.1".to_string(),
            port: addr.port(),
        };
        let result = prober.probe(&target, PROBE_TIMEOUT).await;
        assert!(
            matches!(result, Err(ProbeFailure::Transport(_))),
            "expected Transport, got {result:?}"
        );
    }

    // 192.0.2.1 (TEST-NET-1, RFC 5737) hangs a connect rather than refusing
    // it, confirmed empirically on this machine: the case a bare connect
    // with no timeout would hang on forever.
    #[tokio::test]
    async fn tcp_probe_against_a_non_routable_address_gives_up_within_the_budget() {
        let short = Duration::from_millis(300);
        let prober = OsProber::new(None, BTreeMap::new());
        let target = ProbeTarget::Tcp {
            host: "192.0.2.1".to_string(),
            port: 1,
        };

        let start = std::time::Instant::now();
        let result = prober.probe(&target, short).await;
        let elapsed = start.elapsed();

        // Either failure is correct; which one arrives depends on whether
        // the network blackholes (Timeout) or answers ICMP unreachable
        // (Transport). The bound below is what matters: no timeout means
        // no return at all.
        assert!(
            matches!(
                result,
                Err(ProbeFailure::Timeout | ProbeFailure::Transport(_))
            ),
            "expected Timeout or Transport, got {result:?}"
        );
        assert!(
            elapsed < short * 3,
            "expected the probe to give up within a small multiple of {short:?}, took {elapsed:?}"
        );
    }

    #[cfg(unix)]
    fn exec_target(command: &str) -> ProbeTarget {
        ProbeTarget::Exec {
            command: command.to_string(),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exec_probe_exit_zero_passes() {
        let prober = OsProber::new(None, BTreeMap::new());
        let result = prober.probe(&exec_target("exit 0"), PROBE_TIMEOUT).await;
        assert_eq!(result, Ok(()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exec_probe_nonzero_exit_is_rejected_with_the_code() {
        let prober = OsProber::new(None, BTreeMap::new());
        let result = prober.probe(&exec_target("exit 3"), PROBE_TIMEOUT).await;
        assert_eq!(result, Err(ProbeFailure::Rejected("3".to_string())));
    }

    // Also makes `kill_on_drop(true)` load-bearing: dropping `cmd.status()`'s
    // future on timeout is what actually sends the kill.
    #[cfg(unix)]
    #[tokio::test]
    async fn exec_probe_that_hangs_is_killed_and_times_out() {
        let short = Duration::from_millis(200);
        let prober = OsProber::new(None, BTreeMap::new());

        let start = std::time::Instant::now();
        let result = prober.probe(&exec_target("sleep 5"), short).await;
        let elapsed = start.elapsed();

        assert_eq!(result, Err(ProbeFailure::Timeout));
        assert!(
            elapsed < short * 3,
            "expected the probe to give up within a small multiple of {short:?}, took {elapsed:?}"
        );
    }

    /// How long the forked grandchild in
    /// [`a_timed_out_exec_probe_kills_the_grandchild_too`] sleeps: comfortably
    /// longer than [`REAP_DEADLINE`], so a passing run proves the kill reached
    /// it rather than that it finished on its own; short enough that a run
    /// panicking before [`Reaper`] fires leaves nothing lingering for a whole
    /// CI job.
    #[cfg(unix)]
    const ORPHAN_SLEEP_SECS: u32 = 30;

    /// How long [`assert_reaped`] waits for a pid to leave the process table.
    /// A signal that lands takes milliseconds; this is slack for a loaded
    /// runner, not an expected duration.
    #[cfg(unix)]
    const REAP_DEADLINE: Duration = Duration::from_secs(5);

    /// Last-resort net for a test that PANICS with real processes still
    /// alive, so a failing assertion never leaks a 30-second `sleep` into the
    /// rest of the run.
    ///
    /// Fires ONLY while panicking: on the success path the test has already
    /// proven the pid is gone, and signalling a pid the OS may since have
    /// recycled is a hazard rather than a safety net.
    #[cfg(unix)]
    struct Reaper(i32);

    #[cfg(unix)]
    impl Drop for Reaper {
        fn drop(&mut self) {
            if !std::thread::panicking() {
                return;
            }
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(self.0),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }

    /// Polls `kill(pid, None)` for `ESRCH` instead of sleeping a fixed guess.
    /// `kill(pid, None)` still returns `Ok` for a zombie, so only a
    /// transition all the way to `ESRCH` proves the process is really gone
    /// rather than exited-but-unreaped.
    ///
    /// Copied from `tests/real_runner.rs`'s own `assert_reaped` for the same
    /// reason that one is a copy of `daemon_e2e.rs`'s: an integration binary
    /// is a separate crate and cannot share a `#[cfg(test)]` helper with this
    /// module.
    #[cfg(unix)]
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

    // A leader-only kill (`kill_on_drop`) only reaches the shell; `sleep &`
    // forks a grandchild that only `kill_probe_group`'s group-wide SIGKILL
    // reaches.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_timed_out_exec_probe_kills_the_grandchild_too() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("grandchild.pid");
        // The pid comes back through a file: this probe's stdio is
        // `/dev/null` by design. `wait` (not a second foreground `sleep`)
        // leaves exactly one process for a failing run's `Reaper` to clean
        // up.
        let command = format!(
            "sleep {ORPHAN_SLEEP_SECS} & echo $! > \"{}\"; wait",
            pidfile.display()
        );
        // Three orders of magnitude more than a `sh` fork-and-write needs, so
        // the file is on disk long before the timeout fires, and still fast.
        let short = Duration::from_millis(500);

        let prober = OsProber::new(None, BTreeMap::new());
        let result = prober.probe(&exec_target(&command), short).await;
        assert_eq!(result, Err(ProbeFailure::Timeout));

        let grandchild: i32 = std::fs::read_to_string(&pidfile)
            .expect("fixture precondition: the shell must record its forked child's pid")
            .trim()
            .parse()
            .expect("`echo $!` prints a pid");
        let _reaper = Reaper(grandchild);

        // The assertion the whole test exists for.
        assert_reaped(grandchild, "the probe command's forked child").await;
    }

    // A nonexistent binary name does not exercise this: `sh -c
    // nonexistent_binary` still spawns and reports "not found" via its own
    // exit code (Rejected("127")). A nonexistent `cwd` is what forces
    // `Command::status` itself to fail before any shell runs.
    #[cfg(unix)]
    #[tokio::test]
    async fn exec_probe_that_cannot_be_spawned_at_all_fails_as_transport() {
        let prober = OsProber::new(
            Some(PathBuf::from("/definitely/does/not/exist/shep-probe-test")),
            BTreeMap::new(),
        );
        let result = prober.probe(&exec_target("exit 0"), PROBE_TIMEOUT).await;
        assert!(
            matches!(result, Err(ProbeFailure::Transport(_))),
            "expected Transport, got {result:?}"
        );
    }

    // Canary read from this process's own `HOME`, not written by the test:
    // catches `.envs()` called without a preceding `.env_clear()`.
    #[cfg(unix)]
    #[tokio::test]
    async fn exec_probe_sees_only_the_env_it_was_constructed_with() {
        assert!(
            std::env::var("HOME").is_ok(),
            "fixture precondition: this test process needs a real HOME to prove it does NOT leak"
        );
        let mut env = BTreeMap::new();
        env.insert("SHEP_PROBE_OWN_VAR".to_string(), "expected".to_string());
        let prober = OsProber::new(None, env);
        let command = "test \"$SHEP_PROBE_OWN_VAR\" = expected && [ -z \"${HOME:-}\" ]";
        let result = prober.probe(&exec_target(command), PROBE_TIMEOUT).await;
        assert_eq!(result, Ok(()));
    }

    // Asserted from inside the child: nothing in the parent can read a
    // `Command`'s configured stdio back. `/dev/null` is a character device
    // that is not a terminal; a pipe, file or real terminal each fail one
    // of the two checks.
    #[cfg(unix)]
    #[tokio::test]
    async fn exec_probe_gets_null_stdio_rather_than_the_daemons() {
        let prober = OsProber::new(None, BTreeMap::new());
        let command = "[ -c /dev/fd/0 ] && [ ! -t 0 ] \
                       && [ -c /dev/fd/1 ] && [ ! -t 1 ] \
                       && [ -c /dev/fd/2 ] && [ ! -t 2 ]";
        let result = prober.probe(&exec_target(command), PROBE_TIMEOUT).await;
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn os_prober_is_dyn_compatible() {
        let _: &dyn Prober = &OsProber::new(None, BTreeMap::new());
    }

    #[test]
    fn debug_redacts_env_values_but_shows_the_count() {
        // env may carry secrets (e.g. DATABASE_URL); exact string pinned
        // so a lazy derive(Debug) refactor fails here.
        let mut env = BTreeMap::new();
        env.insert("DATABASE_URL".to_string(), "postgres://secret".to_string());
        env.insert("RUST_LOG".to_string(), "info".to_string());
        let prober = OsProber::new(None, env);
        assert_eq!(
            format!("{prober:?}"),
            "OsProber { cwd: None, env: <2 vars>, .. }"
        );
    }
}
