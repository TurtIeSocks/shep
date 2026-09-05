//! An HTTP server that waits before it starts listening.
//!
//! Watch a `readiness_probe` poll and fail while this sleeps. Watch
//! `listen_timeout` expire if the daemon's fallback window is shorter
//! than the wait. Same server as `http_server`, with one extra step
//! before the bind.
//!
//! # Usage
//!
//! ```text
//! slow_boot <port> <delay-seconds>
//! ```

#![forbid(unsafe_code)]

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

/// How long a connection's read is allowed to block before this program
/// gives up on seeing a full request and answers anyway.
const READ_TIMEOUT: Duration = Duration::from_millis(200);

fn main() {
    let mut args = std::env::args().skip(1);
    let port = parse_port(args.next().as_deref());
    let delay = parse_delay(args.next().as_deref());

    println!(
        "slow_boot pid={} sleeping {delay}s before listening on port {port}",
        std::process::id()
    );
    std::thread::sleep(Duration::from_secs(delay));

    let listener = TcpListener::bind(("127.0.0.1", port))
        .unwrap_or_else(|error| panic!("could not bind 127.0.0.1:{port}: {error}"));
    println!("slow_boot: now listening on 127.0.0.1:{port}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                std::thread::spawn(move || respond(stream));
            }
            Err(error) => eprintln!("slow_boot: accept failed: {error}"),
        }
    }
}

/// Reads whatever is available and writes a fixed `200 OK` response.
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

/// Parses the port argument, or dies naming what was wrong with it.
///
/// # Panics
///
/// Panics if no argument was given or it does not parse as a `u16`.
fn parse_port(raw: Option<&str>) -> u16 {
    let raw = raw.unwrap_or_else(|| panic!("usage: slow_boot <port> <delay-seconds>"));
    raw.parse()
        .unwrap_or_else(|error| panic!("`{raw}` is not a valid port: {error}"))
}

/// Parses the boot delay, in whole seconds.
///
/// # Panics
///
/// Panics if no argument was given or it does not parse as a `u64`.
fn parse_delay(raw: Option<&str>) -> u64 {
    let raw = raw.unwrap_or_else(|| panic!("usage: slow_boot <port> <delay-seconds>"));
    raw.parse()
        .unwrap_or_else(|error| panic!("`{raw}` is not a valid delay in seconds: {error}"))
}
