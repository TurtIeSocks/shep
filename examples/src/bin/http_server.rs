//! A plain HTTP server: binds a port, answers every request `200 OK`.
//!
//! The baseline every probe example in `examples/Flockfile.toml` points at —
//! a `readiness_probe`/`liveness_probe` with `kind = "http"` needs something
//! real to poll. It needs no request parsing to demonstrate that: reading
//! whatever the client sent (best-effort, so a slow or silent client cannot
//! hang a worker thread) is enough to let the response go out and the
//! connection close cleanly.
//!
//! # Usage
//!
//! ```text
//! http_server <port>
//! ```

#![forbid(unsafe_code)]

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

/// How long a connection's read is allowed to block before this program gives
/// up on seeing a full request and answers anyway. A probe or `curl` writes
/// its request in one flush, so this only guards against a client that
/// connects and then sends nothing.
const READ_TIMEOUT: Duration = Duration::from_millis(200);

fn main() {
    let port = parse_port(std::env::args().nth(1).as_deref());
    let listener = TcpListener::bind(("127.0.0.1", port))
        .unwrap_or_else(|error| panic!("could not bind 127.0.0.1:{port}: {error}"));
    println!(
        "http_server pid={} listening on 127.0.0.1:{port}",
        std::process::id()
    );

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                std::thread::spawn(move || respond(stream));
            }
            Err(error) => eprintln!("http_server: accept failed: {error}"),
        }
    }
}

/// Reads whatever is available (ignoring the result — a client that never
/// sends anything just times out) and writes a fixed `200 OK` response.
fn respond(mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let mut buf = [0_u8; 1024];
    let _ = stream.read(&mut buf);

    let body = "OK";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Parses the port argument, or dies naming what was wrong with it — a
/// silently-defaulted port would leave a probe polling the wrong place with
/// no error anywhere.
///
/// # Panics
///
/// Panics if no argument was given or it does not parse as a `u16`.
fn parse_port(raw: Option<&str>) -> u16 {
    let raw = raw.unwrap_or_else(|| panic!("usage: http_server <port>"));
    raw.parse()
        .unwrap_or_else(|error| panic!("`{raw}` is not a valid port: {error}"))
}
