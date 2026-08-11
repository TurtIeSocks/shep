//! A sheep that serves one TCP port with `SO_REUSEPORT`, and that can be told
//! to ignore its stop signal.
//!
//! `tests/daemon_e2e.rs`'s reload measurement starts this program under a real
//! daemon, connects to it continuously, and reloads it — so that what a reload
//! costs a live connection is measured against an application that cooperates
//! and against one that does not. Nothing else in the suite can stand in for
//! it: every other real child here is `/bin/sh` running an inline script, and
//! `/bin/sh` cannot set a socket option.
//!
//! # Why this is an `examples/` target
//!
//! Cargo has no "helper program a test executes" target kind, and each of the
//! three that exist costs something different:
//!
//! - `src/bin/` would be built for every consumer, installed by `cargo install`
//!   and — the deciding one — could not use a dev-dependency, so `nix`'s
//!   `socket` feature would have to join the SHIPPED daemon's dependency graph
//!   to serve a fixture.
//! - A `[[test]]` target is RUN by `cargo test`; a server that never returns is
//!   not a test.
//! - A separate workspace member is not built by `cargo test -p shep-daemon`,
//!   which is a leg of this repo's CI.
//!
//! An example is built by a plain `cargo test`, may use dev-dependencies, and
//! is never installed. It reaches for none of `shep-daemon`'s own API, which is
//! unusual for the target kind and is the price of the other three being worse.
//!
//! # Contract
//!
//! Environment, all read once at startup:
//!
//! - `SHEEP_PORT_BASE` (required) — the sheep binds `127.0.0.1` on
//!   `SHEEP_PORT_BASE + SHEP_INSTANCE`. Derived from the instance slot rather
//!   than taken whole so that a reload replacing an instance in a DIFFERENT
//!   slot would bind a different port, and a caller that keeps connecting to
//!   the old one would see it.
//! - `SHEP_INSTANCE` (required) — the slot, which the daemon sets for every
//!   sheep it spawns.
//! - `SHEEP_HOLD_MS` (optional, default 40) — how long a connection is held
//!   before its reply. A reply is not instant because a handover's cost is
//!   mostly what happens to work already in hand.
//! - `SHEEP_DEFIANT` (optional, `1` to enable) — never act on `SIGTERM`. The
//!   uncooperative half of the measurement.
//!
//! Protocol: the server speaks first, writing `<pid>\n` and closing. The pid
//! is what lets a caller attribute every answered connection to a process,
//! which is how the test tells its own setup races apart from the thing it is
//! measuring.
//!
//! Startup is announced on stdout, so a failing run leaves the port, the pid
//! and the role in the sheep's log.

// nix is unix-only and so is everything below; the Windows CI leg builds this
// file and gets the stub at the bottom. The test that drives it is
// `#![cfg(unix)]` for the same reason.
#[cfg(unix)]
fn main() {
    unix::serve();
}

#[cfg(unix)]
mod unix {
    use std::io::Write as _;
    use std::net::{TcpListener, TcpStream};
    use std::os::fd::AsRawFd as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    use nix::sys::signal::{SigSet, Signal};
    use nix::sys::socket::{
        AddressFamily, Backlog, SockFlag, SockType, SockaddrIn, bind, listen, setsockopt, socket,
        sockopt,
    };

    /// How long the accept loop sleeps between polls of a listener that has
    /// nothing queued. A drain has to notice its stop signal without an
    /// `accept` blocking through it, and a poll this short is invisible next
    /// to the connection rate the test drives.
    const ACCEPT_POLL: Duration = Duration::from_millis(1);

    /// Reads `key`, or dies saying which variable was missing — a fixture that
    /// silently defaulted a port would be measured against the wrong thing.
    fn required(key: &str) -> u16 {
        let raw = std::env::var(key).unwrap_or_else(|_| panic!("{key} must be set"));
        raw.parse()
            .unwrap_or_else(|error| panic!("{key}={raw} is not a port number: {error}"))
    }

    /// Binds, serves, and — unless told to be defiant — drains on `SIGTERM`.
    pub fn serve() {
        let port = required("SHEEP_PORT_BASE") + required("SHEP_INSTANCE");
        let hold = Duration::from_millis(
            std::env::var("SHEEP_HOLD_MS")
                .ok()
                .and_then(|raw| raw.parse().ok())
                .unwrap_or(40),
        );
        let defiant = std::env::var("SHEEP_DEFIANT").is_ok_and(|raw| raw == "1");

        // Blocked before any thread exists, so every thread spawned below
        // inherits the mask and none of them can take the signal instead.
        // Blocking rather than installing SIG_IGN is what keeps this file free
        // of `unsafe`: `sigaction`/`signal` are unsafe in nix, `pthread_sigmask`
        // and `sigwait` are not. A blocked SIGTERM is delivered to nobody and
        // stays pending, which is exactly "ignores its stop signal".
        let mut term = SigSet::empty();
        term.add(Signal::SIGTERM);
        term.thread_block().expect("SIGTERM must be blockable");

        let stopping = Arc::new(AtomicBool::new(false));
        if !defiant {
            let stopping = Arc::clone(&stopping);
            thread::spawn(move || {
                term.wait().expect("sigwait must not fail");
                stopping.store(true, Ordering::Relaxed);
            });
        }

        let listener = bind_reuse_port(port);
        listener
            .set_nonblocking(true)
            .expect("a listener must accept O_NONBLOCK");
        println!(
            "reuse_port_sheep pid={} port={port} defiant={defiant} hold_ms={}",
            std::process::id(),
            hold.as_millis()
        );

        let mut in_flight = Vec::new();
        loop {
            match listener.accept() {
                Ok((conn, _)) => in_flight.push(thread::spawn(move || answer(conn, hold))),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    // The queue is empty. That is the only moment at which
                    // closing the listener costs nothing: a connection already
                    // queued and not yet accepted is RESET when its listener
                    // closes, which is the loss this whole fixture exists to
                    // measure.
                    if stopping.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(ACCEPT_POLL);
                }
                Err(error) => panic!("accept failed: {error}"),
            }
        }

        // Stop accepting first, then finish what was already accepted: that
        // order is what "drain" means, and reversing it would hand the
        // measurement a window in which this process is neither taking new
        // work nor done with the old.
        drop(listener);
        for handle in in_flight {
            let _ = handle.join();
        }
    }

    /// A listening socket on `127.0.0.1:port` with `SO_REUSEPORT` set before
    /// the bind — the option a reload's overlap depends on, and the one shep
    /// cannot set on an app's behalf (`AppConfig::reuse_port` documents that
    /// division).
    ///
    /// `SO_REUSEADDR` rides along for the ordinary reason every server sets it:
    /// this port carries hundreds of short connections per run, and their
    /// `TIME_WAIT` remains would otherwise refuse the next bind.
    ///
    /// Hand-built rather than `TcpListener::bind` because `std` has no way to
    /// set a socket option before binding. Every call here is safe: `socket`
    /// hands back an `OwnedFd`, and `TcpListener: From<OwnedFd>` takes it.
    fn bind_reuse_port(port: u16) -> TcpListener {
        let sock = socket(
            AddressFamily::Inet,
            SockType::Stream,
            SockFlag::empty(),
            None,
        )
        .expect("a TCP socket must be creatable");
        setsockopt(&sock, sockopt::ReuseAddr, &true).expect("SO_REUSEADDR must be settable");
        setsockopt(&sock, sockopt::ReusePort, &true).expect("SO_REUSEPORT must be settable");
        bind(sock.as_raw_fd(), &SockaddrIn::new(127, 0, 0, 1, port))
            .unwrap_or_else(|error| panic!("bind on 127.0.0.1:{port} failed: {error}"));
        // `SOMAXCONN`, not a number of our own: a queue this fixture overflowed
        // would drop connections for a reason that has nothing to do with a
        // reload, and the two losses are indistinguishable from the far end.
        listen(&sock, Backlog::MAXCONN).expect("a bound socket must be able to listen");
        TcpListener::from(sock)
    }

    /// Holds one connection for `hold`, then answers with this process's pid.
    ///
    /// Write errors are ignored: a caller that walked away mid-hold is the
    /// caller's business, and this program's exit status belongs to the daemon
    /// supervising it.
    fn answer(mut conn: TcpStream, hold: Duration) {
        thread::sleep(hold);
        let _ = writeln!(conn, "{}", std::process::id());
        let _ = conn.flush();
    }
}

#[cfg(not(unix))]
fn main() {
    // Reached by nothing: the only caller is a `#![cfg(unix)]` test binary.
    // Present so the Windows leg of the matrix compiles this target.
    eprintln!("reuse_port_sheep needs SO_REUSEPORT and a unix signal mask");
    std::process::exit(1);
}
