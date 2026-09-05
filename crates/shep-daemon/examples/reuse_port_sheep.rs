//! A sheep serving one TCP port with `SO_REUSEPORT`, and able to
//! ignore its stop signal. Used by `daemon_e2e.rs`'s reload
//! measurement, since `/bin/sh` cannot set a socket option.
//!
//! Env, read once at startup: `SHEEP_PORT_BASE` + `SHEP_INSTANCE`
//! (required, bind port); `SHEEP_HOLD_MS` (default 40, reply delay);
//! `SHEEP_DEFIANT=1` (ignore `SIGTERM`); `SHEEP_MUTE_FILE` (if it
//! exists at startup, bind nothing and answer nothing).
//!
//! Protocol: the server writes `<pid>\n` and closes. Startup is
//! announced on stdout.

// nix is unix-only, and so is everything below. The Windows CI leg
// builds this file and gets the stub at the bottom.
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

    /// How long the accept loop sleeps between polls of an empty
    /// listener. Short enough to notice a stop signal promptly, and
    /// invisible next to the test's connection rate.
    const ACCEPT_POLL: Duration = Duration::from_millis(1);

    /// Reads `key`, or dies saying which variable was missing. A
    /// silently defaulted port would measure the wrong thing.
    fn required(key: &str) -> u16 {
        let raw = std::env::var(key).unwrap_or_else(|_| panic!("{key} must be set"));
        raw.parse()
            .unwrap_or_else(|error| panic!("{key}={raw} is not a port number: {error}"))
    }

    /// Binds, serves, and drains on `SIGTERM` unless told to be defiant.
    pub fn serve() {
        let port = required("SHEEP_PORT_BASE") + required("SHEP_INSTANCE");
        let hold = Duration::from_millis(
            std::env::var("SHEEP_HOLD_MS")
                .ok()
                .and_then(|raw| raw.parse().ok())
                .unwrap_or(40),
        );
        let defiant = std::env::var("SHEEP_DEFIANT").is_ok_and(|raw| raw == "1");
        let mute =
            std::env::var("SHEEP_MUTE_FILE").is_ok_and(|path| std::path::Path::new(&path).exists());

        // Blocked before any thread exists, so every spawned thread
        // inherits the mask. Blocking, not `SIG_IGN`, needs no
        // `unsafe`. A blocked SIGTERM stays pending: this is what
        // "ignores its stop signal" means.
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

        if mute {
            // Nothing is bound, so only some other process can answer
            // a probe against this port. A reload that calls this
            // instance ready did so on another instance's answer.
            println!(
                "reuse_port_sheep pid={} port={port} MUTE (bound nothing)",
                std::process::id()
            );
            loop {
                if stopping.load(Ordering::Relaxed) {
                    return;
                }
                thread::sleep(ACCEPT_POLL);
            }
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
                    // The queue is empty: the only moment closing the
                    // listener costs nothing. A connection queued but
                    // not yet accepted resets when its listener
                    // closes.
                    if stopping.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(ACCEPT_POLL);
                }
                Err(error) => panic!("accept failed: {error}"),
            }
        }

        // Stop accepting first, then finish what was already
        // accepted: that order is what "drain" means. Reversing it
        // leaves a window: no new work taken, none of the old
        // finished.
        drop(listener);
        for handle in in_flight {
            let _ = handle.join();
        }
    }

    /// A listening socket on `127.0.0.1:port` with `SO_REUSEPORT` set
    /// before the bind. Shep cannot set this on an app's behalf
    /// (`AppConfig::reuse_port` documents that division).
    ///
    /// `SO_REUSEADDR` rides along too: this port carries hundreds of
    /// short connections per run. Their `TIME_WAIT` remains would
    /// otherwise refuse the next bind.
    ///
    /// Hand-built rather than `TcpListener::bind`, since `std` has no
    /// way to set a socket option before binding. `socket` hands back
    /// an `OwnedFd`, and `TcpListener: From<OwnedFd>` takes it safely.
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
    /// Write errors are ignored: a caller that walked away mid-hold is
    /// the caller's business. This program's exit status belongs to
    /// the daemon supervising it.
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
