//! The metrics dog: [`MetricsConfig`], [`run`], and the data types
//! [`exposition::render`] turns into Prometheus text.
//!
//! [`Reading`] is a snapshot, not a running total: nothing here accumulates
//! across scrapes, so there is no state to leak between requests and no
//! `Mutex` for a slow scraper to hold. [`run`] polls `Request::ListFlock`
//! and the daemon handshake fresh on every `/metrics` request and builds
//! one of these to hand to [`exposition::render`] — never a cached reading
//! refreshed on a timer, which would either serve a stale reading between
//! scrapes or pay the sample when nobody is asking.

pub mod exposition;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use shep_client::Client;
use shep_core::protocol::{ProcessInfo, Request, Response};
use sysinfo::{MemoryRefreshKind, ProcessRefreshKind, RefreshKind, System};
use tokio::net::{TcpListener, TcpStream};
use tokio::signal::unix::{SignalKind, signal};

use super::DogRuntime;
use super::http::{self, HttpError};
use crate::exit::ExitCode;

/// How long [`http::read_request`] waits for a connected peer to finish
/// sending its request before giving up on it. Generous for a scraper (an
/// ordinary HTTP client on the same host), small enough that a peer that
/// connects and says nothing does not hold a task open indefinitely — see
/// `http.rs`'s own `a_peer_that_says_nothing_is_dropped_at_the_timeout`.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// `[dog.metrics]`.
///
/// `deny_unknown_fields`: a misspelled key must be a startup error naming
/// it, not a dog silently serving on a port the operator did not choose.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MetricsConfig {
    /// Where to listen. Loopback by default; binding wider is explicit.
    ///
    /// A metrics endpoint carries every sheep's name, and on many hosts a
    /// sheep's name is the name of an internal service. `0.0.0.0:9615` is
    /// available to an operator who wants it and is never the default —
    /// this dog will not widen its own exposure as a side effect of being
    /// enabled.
    pub bind: SocketAddr,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 9615)),
        }
    }
}

#[cfg(test)]
impl MetricsConfig {
    /// A config bound to `port` on loopback — [`Default`] pins the port to
    /// `9615`; a test that binds a real socket needs `0` instead (the OS
    /// assigns a free one), never a fixed number, which is how a test suite
    /// starts failing on a developer's machine for reasons unrelated to the
    /// change under test.
    fn default_on_port(port: u16) -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], port)),
        }
    }
}

/// What the shepherd and the host looked like when the exposition was
/// rendered.
#[derive(Debug, Default)]
pub struct Reading {
    /// Every registered entry, sheep and dogs alike, as `ListFlock`
    /// answered.
    pub flock: Vec<ProcessInfo>,
    /// The shepherd's crate version, from the handshake rather than from a
    /// request: `HelloAck` already answered it, so asking again would be a
    /// round trip for something in hand (the same reasoning `shep ping`'s
    /// own module records).
    pub daemon_version: String,
    /// The shepherd's pid, from the same handshake.
    pub daemon_pid: u32,
    /// Host totals, `None` where the sampler could not read them.
    pub host: Option<HostReading>,
}

/// The machine the flock is running on.
///
/// Read through `sysinfo`, which is already a workspace dependency —
/// shep-daemon samples every sheep's tree with it — so naming it in
/// shep-cli's manifest adds **zero** crates to the tree (confirmed: see
/// this task's own report for the `cargo tree` counts before and after).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostReading {
    /// Total physical memory in bytes.
    pub memory_total_bytes: u64,
    /// Memory in use, as the platform reports it.
    pub memory_used_bytes: u64,
    /// How many processes the host is running, the flock included. The
    /// number that explains a sampling walk getting slower.
    pub processes: usize,
    /// Seconds since the host booted.
    pub uptime_seconds: u64,
}

/// Reads the host's memory, process count and uptime, fresh — one short-
/// lived `sysinfo::System` per call, since a scrape is already the "ask
/// again" event this dog is built around (see this module's own doc); there
/// is no timer here either.
///
/// `None` on a target `sysinfo` does not support — [`Reading::host`]'s own
/// doc is explicit that this is a real, expected case, not an error a
/// scrape should fail over.
fn sample_host() -> Option<HostReading> {
    if !sysinfo::IS_SUPPORTED_SYSTEM {
        return None;
    }
    let system = System::new_with_specifics(
        RefreshKind::nothing()
            .with_memory(MemoryRefreshKind::everything())
            .with_processes(ProcessRefreshKind::nothing()),
    );
    Some(HostReading {
        memory_total_bytes: system.total_memory(),
        memory_used_bytes: system.used_memory(),
        processes: system.processes().len(),
        uptime_seconds: System::uptime(),
    })
}

/// Runs the metrics dog until it is signalled.
///
/// Binds [`MetricsConfig::bind`] and serves until `SIGINT` or `SIGTERM` —
/// the latter is what the shepherd's own kill ladder actually sends
/// (`shep disable`'s first rung), and a dog that only listens for `SIGINT`
/// rides the whole ladder to `SIGKILL` on every disable, which is slow and
/// looks like a hang.
///
/// A refused bind is fatal: this dog's whole purpose is to serve that port,
/// and a metrics dog that is running but bound to nothing is worse than one
/// `shep dogs` reports as `Errored`, because the first looks fine from the
/// outside.
pub async fn run(runtime: DogRuntime) -> ExitCode {
    let config = match runtime.config::<MetricsConfig>() {
        Ok(config) => config,
        Err(_err) => {
            // The fact, not the value: `DogRunError::Section`'s message is
            // the TOML parser's own complaint, which can quote the
            // offending line — this task's own dispatch brief is explicit
            // that a dog logging about its own configuration logs the
            // fact, never the value, and that rule does not carve out an
            // exception for a config shape (like this one) that happens not
            // to carry a secret today.
            eprintln!("shep dog metrics: [dog.metrics] does not parse; see `shep dogs`");
            return ExitCode::InvalidConfig;
        }
    };
    let listener = match TcpListener::bind(config.bind).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("shep dog metrics: could not bind {}: {err}", config.bind);
            return ExitCode::Failure;
        }
    };
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(sigterm) => sigterm,
        Err(err) => {
            eprintln!("shep dog metrics: could not install a SIGTERM handler: {err}");
            return ExitCode::Failure;
        }
    };
    let client = Arc::new(runtime.client);
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
        () = accept_forever(listener, client) => {}
    }
    ExitCode::Success
}

/// Accepts connections off `listener` forever, one task per connection.
/// Never returns on its own — [`run`] races it against the two shutdown
/// signals, and a test drives it directly, aborting the [`tokio::task::JoinHandle`]
/// it comes back on rather than waiting for a return that never happens.
async fn accept_forever(listener: TcpListener, client: Arc<Client>) {
    loop {
        match listener.accept().await {
            Ok((stream, _peer)) => {
                let client = Arc::clone(&client);
                tokio::spawn(handle_connection(stream, client));
            }
            Err(err) => {
                eprintln!("shep dog metrics: accept failed: {err}");
            }
        }
    }
}

/// Serves exactly one request on `stream`: `/metrics` answers the
/// exposition, everything else answers 404 naming `/metrics` — an operator
/// who curls `/` is told where to look rather than handed the exposition
/// from the wrong path, which is how a scrape config ends up depending on a
/// path the next version does not serve.
async fn handle_connection(mut stream: TcpStream, client: Arc<Client>) {
    let request = match http::read_request(&mut stream, READ_TIMEOUT).await {
        Ok(request) => request,
        // A peer that never finished a request (timed out, sent garbage,
        // grew the head past the ceiling, disconnected mid-read) gets no
        // reply at all — there is no well-formed request to answer, and
        // `read_request`'s own errors (`HttpError`) already log nothing
        // sensitive because it never sees anything but sizes and reasons.
        Err(_err) => return,
    };
    let path = request
        .target
        .split('?')
        .next()
        .unwrap_or(request.target.as_str());
    if path != "/metrics" {
        let _: Result<(), HttpError> = http::write_response(
            &mut stream,
            404,
            "text/plain",
            b"not found; metrics are served at /metrics\n",
        )
        .await;
        return;
    }

    let flock = match client.request(Request::ListFlock).await {
        Ok(Response::Flock(flock)) => flock,
        // A failed `ListFlock` answers 503, not a stale exposition or a 200
        // with nothing in it: a scraper reading a 200 records "the flock is
        // empty," indistinguishable from a real empty flock, while a 503 is
        // `up == 0` for this target — what actually happened.
        Ok(_) | Err(_) => {
            let _: Result<(), HttpError> = http::write_response(
                &mut stream,
                503,
                "text/plain",
                b"the shepherd did not answer\n",
            )
            .await;
            return;
        }
    };

    let reading = Reading {
        flock,
        daemon_version: client.daemon().daemon_version.clone(),
        daemon_pid: client.daemon().pid,
        host: sample_host(),
    };
    let body = exposition::render(&reading);
    let _: Result<(), HttpError> = http::write_response(
        &mut stream,
        200,
        "text/plain; version=0.0.4",
        body.as_bytes(),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use shep_client::testing::{fake_client_on, fake_client_that_dies_mid_request};
    use shep_core::protocol::ProcessInfo;
    use shep_core::status::ProcStatus;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::task::JoinHandle;

    use super::*;

    /// A minimal, online sheep fixture — enough for the exposition to name
    /// it in a `sheep="..."` label, nothing more precise than that.
    fn sample_info(name: &str) -> ProcessInfo {
        ProcessInfo {
            id: 1,
            name: name.to_string(),
            status: ProcStatus::Online,
            pid: Some(4242),
            restarts: 0,
            uptime_ms: 1_000,
            fold: None,
            out_file: None,
            err_file: None,
            cpu_percent: Some(0.5),
            memory_bytes: Some(1024),
            dog: None,
        }
    }

    /// A running metrics dog bound to an OS-assigned loopback port, backed
    /// by `client` — the connection a test's fake shepherd double is on the
    /// other end of. Aborts its serving task on drop, so a dog left
    /// listening past its own test cannot hold a port for the rest of the
    /// binary's run.
    struct RunningDog {
        addr: SocketAddr,
        handle: JoinHandle<()>,
    }

    impl RunningDog {
        fn addr(&self) -> SocketAddr {
            self.addr
        }
    }

    impl Drop for RunningDog {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    /// Binds `config.bind` (port `0` in every test below — never a fixed
    /// port, which is how a test suite starts failing on a developer's
    /// machine for reasons unrelated to the change), reads back the
    /// OS-assigned address, and serves `client` on it in the background.
    async fn serve_on_free_port(client: Client, config: MetricsConfig) -> RunningDog {
        let listener = TcpListener::bind(config.bind).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(accept_forever(listener, Arc::new(client)));
        RunningDog { addr, handle }
    }

    /// Connects to `addr`, sends a bare `GET <path>` and returns the raw
    /// response text (status line, headers and body together) — enough for
    /// a test to assert on the status line and on a body substring without
    /// a second helper for each. Every read/write is wrapped in a generous
    /// timeout: a test that hangs here is a bug in the dog under test, not
    /// a reason to hang the suite.
    async fn scrape(addr: SocketAddr, path: &str) -> String {
        let mut stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr))
            .await
            .expect("connect must not hang")
            .unwrap();
        let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        tokio::time::timeout(Duration::from_secs(5), stream.write_all(request.as_bytes()))
            .await
            .expect("write must not hang")
            .unwrap();
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf))
            .await
            .expect("read must not hang")
            .unwrap();
        String::from_utf8(buf).expect("the exposition is ASCII/UTF-8")
    }

    /// fails if a scrape is served from a cached reading. The fake shepherd
    /// answers a DIFFERENT flock to the second `ListFlock`, so a dog that
    /// polled once at startup serves the first one twice and reddens here.
    /// A cached reading is not a hypothetical shortcut — it is what a dog
    /// written around a refresh timer does, and it is invisible while the
    /// flock happens not to change.
    #[tokio::test]
    async fn every_scrape_asks_the_shepherd_again() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("s.sock");
        let (client, daemon) = fake_client_on(&socket).await;
        daemon.reply_to_list_sequence(vec![
            vec![sample_info("web")],
            vec![sample_info("web"), sample_info("api")],
        ]);
        let dog = serve_on_free_port(client, MetricsConfig::default_on_port(0)).await;

        let first = scrape(dog.addr(), "/metrics").await;
        assert!(first.contains(r#"sheep="web""#), "{first}");
        assert!(!first.contains(r#"sheep="api""#), "{first}");

        let second = scrape(dog.addr(), "/metrics").await;
        assert!(
            second.contains(r#"sheep="api""#),
            "the second scrape must see the second listing: {second}"
        );
        assert_eq!(daemon.list_flock_count(), 2, "one ListFlock per scrape");
    }

    /// fails if the default bind is anything but loopback. A metrics
    /// endpoint carries every sheep's name; widening it must be the
    /// operator's explicit act and never a consequence of `shep enable`.
    #[test]
    fn the_default_bind_is_loopback() {
        assert_eq!(
            MetricsConfig::default().bind,
            "127.0.0.1:9615".parse::<SocketAddr>().unwrap()
        );
        // And an empty section — the ordinary case for a dog nobody
        // configured — resolves to that same default rather than to
        // `0.0.0.0`, which a `Default` derived on `SocketAddr` would give.
        let parsed: MetricsConfig = toml::from_str("").unwrap();
        assert_eq!(parsed, MetricsConfig::default());
    }

    /// fails if a shepherd that will not answer produces a 200. A scraper
    /// reading a 200 with an empty body records an empty flock, which is
    /// indistinguishable from a real one; a 503 is `up == 0`, which is what
    /// happened.
    ///
    /// `fake_client_that_dies_mid_request` reads exactly one envelope (this
    /// dog's own `ListFlock`) and drops the connection without a reply —
    /// the shepherd accepted the request and then never answered it, which
    /// is "will not answer" without needing a real timeout to prove it.
    #[tokio::test]
    async fn a_shepherd_that_will_not_answer_produces_a_503() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("s.sock");
        let (client, _task) = fake_client_that_dies_mid_request(&socket).await;
        let dog = serve_on_free_port(client, MetricsConfig::default_on_port(0)).await;

        let response = scrape(dog.addr(), "/metrics").await;
        assert!(response.starts_with("HTTP/1.1 503 "), "{response}");
    }

    /// fails if any path serves the exposition. A scrape config that
    /// happens to work against `/` is a scrape config that breaks the day
    /// the path is honoured.
    #[tokio::test]
    async fn only_the_metrics_path_serves_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("s.sock");
        let (client, _daemon) = fake_client_on(&socket).await;
        let dog = serve_on_free_port(client, MetricsConfig::default_on_port(0)).await;

        let root = scrape(dog.addr(), "/").await;
        assert!(root.starts_with("HTTP/1.1 404 "), "{root}");
        assert!(root.contains("/metrics"), "{root}");

        let metrics = scrape(dog.addr(), "/metrics").await;
        assert!(metrics.starts_with("HTTP/1.1 200 "), "{metrics}");
    }
}
