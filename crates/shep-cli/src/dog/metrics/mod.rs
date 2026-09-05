//! The metrics dog: [`MetricsConfig`], [`run`], and the data types
//! [`exposition::render`] turns into Prometheus text.
//!
//! [`Reading`] is a snapshot, not a running total: nothing accumulates
//! across scrapes, so there is no state to leak between requests and no
//! `Mutex` for a slow scraper to hold. [`run`] polls `Request::ListFlock`
//! and the handshake fresh on every `/metrics` request; nothing here is
//! cached or refreshed on a timer.

pub mod exposition;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use shep_client::ReconnectingClient;
use shep_client::dogs::DogConfig;
use shep_core::protocol::{ProcessInfo, Request, Response};
use sysinfo::{MemoryRefreshKind, ProcessRefreshKind, RefreshKind, System};
use tokio::net::{TcpListener, TcpStream};

use super::DogRuntime;
use crate::exit::ExitCode;
use crate::http::{self, HttpError};

/// How long [`http::read_request`] waits for a connected peer to finish
/// sending its request. Generous for a scraper on the same host, small
/// enough that a peer that connects and says nothing cannot hold a task
/// open indefinitely.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// `[dog.metrics]`.
///
/// `deny_unknown_fields`: a misspelled key must be a startup error naming
/// it, not a dog silently serving on a port the operator did not choose.
///
/// `DogConfig` carries no `#[shep(secret)]`, because an address is not a
/// credential. The derive is still what lets `config_schema` publish this
/// section, and a config type with nothing to mark still wants the impl.
#[derive(Debug, Clone, PartialEq, Deserialize, schemars::JsonSchema, DogConfig)]
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
    /// A config bound to `port` on loopback. [`Default`] pins `9615`; a
    /// test that binds a real socket passes `0` so the OS assigns one.
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
    /// request: `HelloAck` already answered it.
    pub daemon_version: String,
    /// The shepherd's pid, from the same handshake.
    pub daemon_pid: u32,
    /// Host totals, `None` where the sampler could not read them.
    pub host: Option<HostReading>,
}

/// The machine the flock is running on, read through `sysinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostReading {
    /// Total physical memory in bytes.
    pub memory_total_bytes: u64,
    /// Memory in use, as the platform reports it.
    pub memory_used_bytes: u64,
    /// How many processes the host is running, the flock included.
    pub processes: usize,
    /// Seconds since the host booted.
    pub uptime_seconds: u64,
}

/// Reads the host's memory, process count and uptime through one
/// short-lived `sysinfo::System` per call.
///
/// `None` on a target `sysinfo` does not support, which is expected
/// rather than an error a scrape should fail over.
///
/// `pub(crate)`: `whistle::read` also calls this for its own host sample.
pub(crate) fn sample_host() -> Option<HostReading> {
    if !sysinfo::IS_SUPPORTED_SYSTEM {
        return None;
    }
    let system = System::new_with_specifics(
        RefreshKind::nothing()
            .with_memory(MemoryRefreshKind::everything())
            // `nothing()` alone still leaves `tasks` on: sysinfo's Linux
            // backend then counts every thread as a process.
            .with_processes(ProcessRefreshKind::nothing().without_tasks()),
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
/// Binds [`MetricsConfig::bind`] and serves until `SIGINT` or `SIGTERM`.
/// `SIGTERM` is the first rung of the shepherd's kill ladder, so a dog
/// deaf to it rides that ladder to `SIGKILL` on every `shep disable`.
///
/// A refused bind is fatal: a dog running but bound to nothing looks
/// healthy from the outside.
pub async fn run(runtime: DogRuntime) -> ExitCode {
    let config = match runtime.config::<MetricsConfig>() {
        Ok(config) => config,
        Err(_err) => {
            // The fact, not the value: `DogRunError::Section`'s message is
            // the TOML parser's own complaint, which can quote the
            // offending line.
            eprintln!("shep dog metrics: [metrics] in dogs.toml does not parse; see `shep dogs`");
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
    let mut sigterm = match crate::shutdown::Terminate::install() {
        Ok(sigterm) => sigterm,
        Err(err) => {
            eprintln!("shep dog metrics: could not install a shutdown handler: {err}");
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
/// Never returns: [`run`] races it against the two shutdown signals.
async fn accept_forever(listener: TcpListener, client: Arc<ReconnectingClient>) {
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
/// exposition, everything else answers 404 naming `/metrics`.
async fn handle_connection(mut stream: TcpStream, client: Arc<ReconnectingClient>) {
    let request = match http::read_request(&mut stream, READ_TIMEOUT).await {
        Ok(request) => request,
        // A peer that never finished a request gets no reply at all:
        // there is no well-formed request to answer.
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
        // A failed `ListFlock` answers 503, not a 200 with nothing in it:
        // a scraper reads that as a real empty flock, where a 503 is
        // `up == 0` for this target.
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

    // Read once, from the generation that just answered `ListFlock`.
    // `ReconnectingClient::daemon` reports the daemon answering now, so two
    // reads either side of a handover could publish one daemon's version
    // beside another's pid.
    let ack = client.daemon();
    let reading = Reading {
        flock,
        daemon_version: ack.daemon_version,
        daemon_pid: ack.pid,
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

    use shep_client::testing::{
        Handshake, fake_daemon_across_handovers, fake_reconnecting_client_on, sample_ack,
    };
    use shep_core::protocol::{HelloAck, PROTOCOL_VERSION, ProcessInfo};
    use shep_core::status::ProcStatus;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::task::JoinHandle;

    use super::*;

    /// `deny_unknown_fields` becomes `additionalProperties: false`;
    /// `default` drops `required` and puts the real default beside the key.
    #[test]
    fn the_metrics_schema_offers_the_port_it_would_bind_and_refuses_unknown_keys() {
        let schema = shep_client::dogs::config_schema::<MetricsConfig>()
            .expect("this config marks no field, so no mark can be missing");
        let schema = schema.as_value();

        assert_eq!(
            schema.get("additionalProperties"),
            Some(&serde_json::Value::Bool(false)),
            "`deny_unknown_fields` is what catches a misspelled key"
        );
        assert_eq!(
            schema.get("required"),
            None,
            "`default` on the type makes every key optional"
        );
        assert_eq!(
            schema.pointer("/properties/bind/default"),
            Some(&serde_json::Value::String("127.0.0.1:9615".to_owned())),
            "the default is loopback, and a pane shows the port rather than a blank"
        );
    }

    /// A minimal, online sheep fixture, enough for the exposition to name
    /// it in a `sheep="..."` label.
    fn sample_info(name: &str) -> ProcessInfo {
        ProcessInfo::builder(1, name, ProcStatus::Online)
            .pid(Some(4242))
            .uptime_ms(1_000)
            .cpu_percent(Some(0.5))
            .memory_bytes(Some(1024))
            .build()
    }

    /// A running metrics dog bound to an OS-assigned loopback port, backed
    /// by `client`. Aborts its serving task on drop, so a dog left
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

    /// Binds `config.bind` (port `0` in every test below, never a fixed
    /// one), reads back the OS-assigned address, and serves `client` on it
    /// in the background.
    async fn serve_on_free_port(client: ReconnectingClient, config: MetricsConfig) -> RunningDog {
        let listener = TcpListener::bind(config.bind).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(accept_forever(listener, Arc::new(client)));
        RunningDog { addr, handle }
    }

    /// Connects to `addr`, sends a bare `GET <path>` and returns the raw
    /// response text: status line, headers and body together. Every
    /// read/write is wrapped in a generous timeout, so a hang here is a bug
    /// in the dog under test, not a reason to hang the suite.
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

    /// The fake shepherd answers a different flock to the second
    /// `ListFlock`, so a dog that polled once at startup serves the first
    /// one twice.
    #[tokio::test]
    async fn every_scrape_asks_the_shepherd_again() {
        let dir = tempfile::tempdir().unwrap();
        let socket = shep_client::testing::control_address(dir.path());
        let (client, daemon) = fake_reconnecting_client_on(&socket).await;
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

    #[test]
    fn the_default_bind_is_loopback() {
        assert_eq!(
            MetricsConfig::default().bind,
            "127.0.0.1:9615".parse::<SocketAddr>().unwrap()
        );
        // An empty section resolves to that same default, not to
        // `0.0.0.0`, which a `Default` derived on `SocketAddr` would give.
        let parsed: MetricsConfig = toml::from_str("").unwrap();
        assert_eq!(parsed, MetricsConfig::default());
    }

    /// `cut_on_next_request` makes the shepherd read exactly one envelope
    /// and drop the connection without a reply: accepted, then never
    /// answered. This is also the shape a daemon handover produces, so it
    /// pins that the request in flight still fails; only the connection
    /// is re-established.
    #[tokio::test]
    async fn a_shepherd_that_will_not_answer_produces_a_503() {
        let dir = tempfile::tempdir().unwrap();
        let socket = shep_client::testing::control_address(dir.path());
        let shepherds =
            fake_daemon_across_handovers(&socket, vec![Handshake::Accept(sample_ack())]);
        let client = ReconnectingClient::connect(&socket).await.unwrap();
        let dog = serve_on_free_port(client, MetricsConfig::default_on_port(0)).await;
        shepherds.cut_on_next_request();

        let response = scrape(dog.addr(), "/metrics").await;
        assert!(response.starts_with("HTTP/1.1 503 "), "{response}");
    }

    /// A dog's process crosses the shepherd's `execve` for free, but only
    /// the listening socket crosses with it, so the accepted connection
    /// this dog holds on the old shepherd dies. A pid check cannot see
    /// that; the assertion here is a scrape instead.
    #[tokio::test]
    async fn a_scrape_after_a_handover_serves_the_successors_flock() {
        let dir = tempfile::tempdir().unwrap();
        let socket = shep_client::testing::control_address(dir.path());
        let successor = HelloAck {
            daemon_version: "0.2.0".into(),
            protocol: PROTOCOL_VERSION,
            pid: 5150,
        };
        let shepherds = fake_daemon_across_handovers(
            &socket,
            vec![
                Handshake::Accept(sample_ack()),
                Handshake::Accept(successor),
            ],
        );
        shepherds.reply_to_list(vec![sample_info("web")]);
        let client = ReconnectingClient::connect(&socket).await.unwrap();
        let dog = serve_on_free_port(client, MetricsConfig::default_on_port(0)).await;

        let before = scrape(dog.addr(), "/metrics").await;
        assert!(before.starts_with("HTTP/1.1 200 "), "{before}");
        assert!(
            before.contains(r#"shep_daemon_up{version="9.9.9"}"#),
            "{before}"
        );

        shepherds.cut().await;

        // Scraped until the successor answers: a scrape landing inside the
        // reconnect window legitimately 503s, and the contract is that the
        // dog recovers, not that it never misses a beat.
        let recovered = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let body = scrape(dog.addr(), "/metrics").await;
                if body.starts_with("HTTP/1.1 200 ") {
                    return body;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the dog must answer 200 again after its shepherd is replaced");

        assert!(
            recovered.contains(r#"sheep="web""#),
            "the exposition must carry the successor's flock: {recovered}"
        );
        assert!(
            recovered.contains(r#"shep_daemon_up{version="0.2.0"}"#),
            "the exposition must name the daemon now running, not the one              that was replaced: {recovered}"
        );
    }

    #[tokio::test]
    async fn only_the_metrics_path_serves_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let socket = shep_client::testing::control_address(dir.path());
        let (client, _daemon) = fake_reconnecting_client_on(&socket).await;
        let dog = serve_on_free_port(client, MetricsConfig::default_on_port(0)).await;

        let root = scrape(dog.addr(), "/").await;
        assert!(root.starts_with("HTTP/1.1 404 "), "{root}");
        assert!(root.contains("/metrics"), "{root}");

        let metrics = scrape(dog.addr(), "/metrics").await;
        assert!(metrics.starts_with("HTTP/1.1 200 "), "{metrics}");
    }
}
