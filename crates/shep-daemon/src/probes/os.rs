//! [`OsProber`] — HTTP, TCP and exec probes over real sockets and processes.
//!
//! The HTTP probe is hand-rolled over `tokio::net::TcpStream` rather than
//! built on a client crate. No TLS and no redirects, and both are visible to
//! the user rather than silent: `https://` targets are rejected at config
//! time
//! ([`ProbeTargetError::HttpsUnsupported`](shep_core::config::ProbeTargetError::HttpsUnsupported)),
//! and a `301` is a [`ProbeFailure::Rejected`], never followed — a probe
//! that follows redirects is a probe that can pass against a completely
//! different service.

// Rejected alternatives, so nobody re-litigates: `reqwest` brings tower and a
// TLS stack into a daemon targeting single-digit-MB idle RSS (spec §14.11);
// `hyper` + `hyper-util` + `http-body-util` is three dependencies and a
// connection-pool abstraction to send one request with `Connection: close`;
// `ureq` and `minreq` are blocking, and a blocking read cannot be cancelled
// by `tokio::time::timeout` — the timeout would return while the thread
// stayed stuck. The `Prober` seam means swapping any of them in later
// touches one file (IR-31).

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
/// An HTTP/1.1 status line is a method-agnostic `HTTP/1.1 200 OK` — tens of
/// bytes. This bound exists so a probe target that is not an HTTP server
/// cannot stream unbounded data into the daemon's heap; nothing legitimate
/// comes close to it.
const HTTP_STATUS_LINE_CAP: u64 = 8 * 1024;

/// `Prober` over real sockets and real processes.
pub struct OsProber {
    /// Working directory for exec probes — `None` inherits the daemon's own,
    /// matching `Command::current_dir`'s own default.
    cwd: Option<PathBuf>,
    /// Environment for exec probes — a probe usually needs the same `PORT`
    /// the sheep was given.
    ///
    /// `Debug` does not leak environment values (IR-41).
    env: BTreeMap<String, String>,
}

impl OsProber {
    /// A prober that runs exec probes in `cwd` with `env`.
    #[must_use]
    pub fn new(cwd: Option<PathBuf>, env: BTreeMap<String, String>) -> Self {
        Self { cwd, env }
    }
}

/// Debug implementation does not leak env values (IR-41): mirrors
/// `AppConfig`'s manual `Debug` (`crates/shep-core/src/config/app.rs`) —
/// one redaction spelling in the workspace, not two.
impl fmt::Debug for OsProber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OsProber")
            .field("cwd", &self.cwd)
            .field("env", &format_args!("<{} vars>", self.env.len()))
            .finish()
    }
}

impl Prober for OsProber {
    fn probe<'a>(
        &'a self,
        target: &'a ProbeTarget,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProbeFailure>> + Send + 'a>> {
        // The `ProbeTarget` match is the compile-time gate (Global
        // Constraints rule 6): `ProbeTarget` is deliberately not
        // `#[non_exhaustive]`, so a fourth transport fails `cargo check`
        // with E0004 at exactly the place that has to change, and no `_`
        // arm may be added here.
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

        // Mandatory: without it, a probe that times out leaves the child
        // running — a 10s interval against a command that takes 30s
        // accumulates processes until the box falls over.
        cmd.kill_on_drop(true);
        // A probe's output is the probe's business, never the daemon's. The
        // default is inheritance, which puts a `curl`-style probe's entire
        // response body — bearer tokens, session cookies, whatever the
        // endpoint returns — into the daemon's own stdout once per interval,
        // forever. The verdict this prober needs is the exit status alone,
        // so there is nothing to read and no reason to keep a pipe.
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        // The probe sees the sheep's environment, never the daemon's —
        // `SpawnSpec::env`'s "no daemon-env leakage beyond this map"
        // (`crates/shep-daemon/src/runner.rs`) already rules this out for
        // sheep, and a probe is not a special case.
        cmd.env_clear().envs(&self.env);

        match tokio::time::timeout(timeout, cmd.status()).await {
            Ok(Ok(status)) if status.success() => Ok(()),
            Ok(Ok(status)) => Err(ProbeFailure::Rejected(exit_code_text(&status))),
            Ok(Err(err)) => Err(ProbeFailure::Transport(err.to_string())),
            Err(_elapsed) => Err(ProbeFailure::Timeout),
        }
    }
}

/// Renders an `ExitStatus` for [`ProbeFailure::Rejected`]. `code()` is
/// `None` on unix when the child died from a signal rather than exiting —
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
/// Connect, write and read are wrapped in ONE `tokio::time::timeout` rather
/// than one per step — three separate timeouts add up to three times the
/// budget the caller configured.
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
    // `(host, port)`, not a formatted `"{host}:{port}"` parsed into a
    // `SocketAddr`: `ProbeTarget` strips brackets from a bracketed IPv6
    // literal at parse time, and a bracket-stripped host is exactly what
    // this tuple form needs — it fails as a formatted string, which has no
    // brackets left to make it parseable (Task 2's obligation).
    let mut stream = TcpStream::connect((host, port))
        .await
        .map_err(|err| ProbeFailure::Transport(err.to_string()))?;

    let header_host = bracket_ipv6(host);
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: {header_host}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|err| ProbeFailure::Transport(err.to_string()))?;

    let status_line = read_status_line(stream).await?;
    evaluate_status_line(&status_line)
}

/// Re-brackets an IPv6 host for the RFC 7230 `Host:` header. `ProbeTarget`
/// strips the brackets off `[::1]` at parse time so `TcpStream::connect` can
/// use `(host, port)` directly (Task 2's obligation); the header needs them
/// back, or `Host: ::1` reads as colon-separated fields instead of one
/// address.
fn bracket_ipv6(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
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

/// Parses the numeric status out of an HTTP status line's second
/// space-separated token (`HTTP/1.1 200 OK` -> `200`) and maps it to a pass
/// or a [`ProbeFailure::Rejected`].
///
/// Never indexes a token that might not be there and never panics on a
/// malformed line: `.nth(1)` and `.parse().ok()` both return `None` rather
/// than panicking, and a probe target is arbitrary, untrusted text — this is
/// the one place a probe genuinely sees "what came back."
///
/// A line with no parseable status is `Rejected` rather than `Transport`:
/// the connection succeeded and bytes came back, so a verdict *was*
/// possible — it was just negative, exactly like a non-2xx status.
fn evaluate_status_line(line: &str) -> Result<(), ProbeFailure> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    match trimmed
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
    // IR-33: real time, not the paused clock. Every test below connects to a
    // real `127.0.0.1` listener, or spawns a real child process. `#[tokio::
    // test(start_paused = true)]` freezes only `tokio::time`; the kernel's
    // TCP stack and process table keep running on the real clock regardless,
    // so a paused test waiting on a real connect, a real accept, or a real
    // child exit would deadlock — the clock inside the test's own task never
    // appears to move, while the other side of the socket or process is
    // unaffected and just sits there. Every `#[tokio::test]` below is a
    // plain one, on purpose.

    use core::time::Duration;

    use std::collections::BTreeMap;

    use tokio::net::TcpListener;

    use super::*;
    use crate::testing::{HttpReply, loopback_http};

    /// Every test's probe timeout — generous enough that CI/loaded-machine
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

    // fails if the status check is `== 200` instead of the documented
    // `200..=299` range
    #[tokio::test]
    async fn passing_status_codes_are_accepted_across_the_2xx_range() {
        for code in [200u16, 204, 299] {
            let (addr, handle) = loopback_http(vec![HttpReply::Status(code)]).await;
            let prober = OsProber::new(None, BTreeMap::new());
            let result = prober
                .probe(&http_target(addr.port(), "/"), PROBE_TIMEOUT)
                .await;
            assert_eq!(result, Ok(()), "status {code} should pass");
            handle.abort();
        }
    }

    // fails if a prober follows the redirect (or otherwise treats 3xx as
    // success) instead of reporting it as a plain rejection — a probe that
    // follows redirects can pass against a completely different service
    #[tokio::test]
    async fn a_301_is_rejected_not_followed() {
        let (addr, handle) = loopback_http(vec![HttpReply::Status(301)]).await;
        let prober = OsProber::new(None, BTreeMap::new());
        let result = prober
            .probe(&http_target(addr.port(), "/"), PROBE_TIMEOUT)
            .await;
        assert_eq!(result, Err(ProbeFailure::Rejected("301".to_string())));
        handle.abort();
    }

    // fails if the prober only checks whether ANY bytes arrived rather than
    // parsing the actual status code
    #[tokio::test]
    async fn a_500_is_rejected() {
        let (addr, handle) = loopback_http(vec![HttpReply::Status(500)]).await;
        let prober = OsProber::new(None, BTreeMap::new());
        let result = prober
            .probe(&http_target(addr.port(), "/"), PROBE_TIMEOUT)
            .await;
        assert_eq!(result, Err(ProbeFailure::Rejected("500".to_string())));
        handle.abort();
    }

    // fails if connect/write/read each get their own `timeout` instead of
    // sharing one: three separate timeouts of `short` would let this test
    // take up to 3x `short` instead of resting at very roughly `short`
    #[tokio::test]
    async fn a_hanging_response_times_out_within_a_small_multiple_of_the_budget() {
        let (addr, handle) = loopback_http(vec![HttpReply::Hang]).await;
        let short = Duration::from_millis(150);
        let prober = OsProber::new(None, BTreeMap::new());

        let start = std::time::Instant::now();
        let result = prober.probe(&http_target(addr.port(), "/"), short).await;
        let elapsed = start.elapsed();

        assert_eq!(result, Err(ProbeFailure::Timeout));
        assert!(
            elapsed < short * 3,
            "expected the probe to give up within a small multiple of {short:?}, took {elapsed:?}"
        );
        handle.abort();
    }

    // fails if a refused connection is collapsed into Timeout instead of
    // Transport — a down service must not look like a slow one
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

    // fails if the status-line parser panics on a malformed line (e.g.
    // unwrapping or indexing a token that is not there) instead of failing
    // it as a value. Asserts Rejected specifically (the judgment call this
    // task made among the brief's two allowed outcomes): the connection
    // succeeded and bytes came back, so a verdict was possible — it was
    // just negative.
    #[tokio::test]
    async fn a_garbage_first_line_is_rejected_not_panicked_on() {
        let (addr, handle) = loopback_http(vec![HttpReply::Raw("not http\r\n".to_string())]).await;
        let prober = OsProber::new(None, BTreeMap::new());
        let result = prober
            .probe(&http_target(addr.port(), "/"), PROBE_TIMEOUT)
            .await;
        assert!(
            matches!(result, Err(ProbeFailure::Rejected(_))),
            "expected Rejected, got {result:?}"
        );
        handle.abort();
    }

    // The brief's own suggested fixture ("not http\r\n") has two
    // space-separated tokens, so it only exercises a parser whose second
    // token fails to parse as u16 — never one that indexes past the end of
    // the token list. This one has none, and is what actually catches an
    // implementation that reaches for `tokens[1]` instead of `.nth(1)`.
    #[tokio::test]
    async fn a_single_token_garbage_line_is_rejected_not_panicked_on() {
        let (addr, handle) = loopback_http(vec![HttpReply::Raw("garbage\r\n".to_string())]).await;
        let prober = OsProber::new(None, BTreeMap::new());
        let result = prober
            .probe(&http_target(addr.port(), "/"), PROBE_TIMEOUT)
            .await;
        assert!(
            matches!(result, Err(ProbeFailure::Rejected(_))),
            "expected Rejected, got {result:?}"
        );
        handle.abort();
    }

    // fails if a TCP probe reports success against any resolvable address
    // instead of actually attempting (and requiring) a real connect
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

    // fails if the TCP probe's connect is not itself wrapped in `timeout` —
    // 192.0.2.1 is TEST-NET-1 (RFC 5737): reserved, unroutable, and
    // confirmed empirically on this machine to hang a connect attempt
    // rather than refuse it immediately (`nc -w2 192.0.2.1 1` blocks for the
    // full 2s rather than erroring at once), which is exactly the "down
    // service that looks slow, not refused" shape a bare `connect().await`
    // with no timeout would hang on forever.
    #[tokio::test]
    async fn tcp_probe_against_a_non_routable_address_times_out() {
        let short = Duration::from_millis(300);
        let prober = OsProber::new(None, BTreeMap::new());
        let target = ProbeTarget::Tcp {
            host: "192.0.2.1".to_string(),
            port: 1,
        };

        let start = std::time::Instant::now();
        let result = prober.probe(&target, short).await;
        let elapsed = start.elapsed();

        assert_eq!(result, Err(ProbeFailure::Timeout));
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

    // fails if the exit code is dropped or hardcoded instead of carried
    // through to Rejected
    #[cfg(unix)]
    #[tokio::test]
    async fn exec_probe_nonzero_exit_is_rejected_with_the_code() {
        let prober = OsProber::new(None, BTreeMap::new());
        let result = prober.probe(&exec_target("exit 3"), PROBE_TIMEOUT).await;
        assert_eq!(result, Err(ProbeFailure::Rejected("3".to_string())));
    }

    // fails if the exec probe's `cmd.status()` await is not itself wrapped
    // in `timeout` — a bare await on a 5s sleep would make this test take
    // ~5s instead of resting near `short`. Also the test that makes
    // `kill_on_drop(true)` load-bearing rather than decorative: dropping
    // `cmd.status()`'s future when `tokio::time::timeout` gives up is what
    // actually sends the kill, and this is the one test in this module
    // whose child would otherwise keep running for seconds after the probe
    // itself has already reported failure.
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

    // fails if a spawn-level failure (the probe is misconfigured, not the
    // app unhealthy) is conflated with an ordinary nonzero exit. A command
    // string naming a genuinely nonexistent binary does NOT exercise this:
    // confirmed empirically (`sh -c nonexistent_binary; echo $?` -> 127)
    // that `sh` itself always spawns and reports "not found" via its OWN
    // exit code, which this prober correctly treats as Rejected("127") per
    // the brief's own flat rule ("anything else is Rejected"), not as a
    // spawn failure. A nonexistent `cwd` is what forces spawn itself
    // (`Command::status`, which chdir's before exec) to return an `Err`
    // before any shell ever runs — the actually-misconfigured-probe case.
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

    // fails if `.envs()` is called without a preceding `.env_clear()` (the
    // canary, inherited from this test process's own real environment,
    // would then leak into the child alongside the prober's own var), and
    // fails if the prober ignores its own env entirely (the own var would
    // never appear). Does not mutate the real process environment (no
    // `std::env::set_var`): the canary is read from THIS process's already-
    // set `HOME`, which every dev machine and CI runner sets, rather than
    // written by the test.
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

    // fails if the exec probe leaves stdio inherited, which is `Command`'s
    // default: a `curl`-style probe then writes its whole response body —
    // bearer tokens and all — into the daemon's own stdout once per interval.
    //
    // Asserted from INSIDE the child because nothing in the parent can read a
    // `Command`'s configured stdio back, and because libtest's capture swaps
    // a thread-local rather than fd 1 — an inherited child really does write
    // to the harness's own stdout. `/dev/null` is a character device that is
    // not a terminal; every realistic inherited stdout is a pipe (CI, any
    // captured run), a regular file (`cargo test > log`) or a terminal, and
    // each of those fails one of the two checks.
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

    // IR-10 dyn-compatibility smoke test.
    #[test]
    fn os_prober_is_dyn_compatible() {
        let _: &dyn Prober = &OsProber::new(None, BTreeMap::new());
    }

    #[test]
    fn debug_redacts_env_values_but_shows_the_count() {
        // IR-41: env may carry the sheep's secrets (e.g. DATABASE_URL).
        // Exact string pinned so a lazy derive(Debug) refactor fails here.
        let mut env = BTreeMap::new();
        env.insert("DATABASE_URL".to_string(), "postgres://secret".to_string());
        env.insert("RUST_LOG".to_string(), "info".to_string());
        let prober = OsProber::new(None, env);
        assert_eq!(
            format!("{prober:?}"),
            "OsProber { cwd: None, env: <2 vars> }"
        );
    }
}
