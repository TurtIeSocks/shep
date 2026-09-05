//! Traps `SIGTERM` and refuses to die, so `kill_timeout` elapses and
//! shep escalates to `SIGKILL`. `graceful_timeout` governs a reload's
//! drain window for the old instance. `kill_timeout` is the grace
//! period between a stop signal and `SIGKILL`, set below in
//! `examples/Flockfile.toml`.
//!
//! Stop this one with `shep stop` to watch `kill_timeout` expire and
//! `SIGKILL` follow.
//!
//! # Usage
//!
//! ```text
//! stubborn
//! ```
//!
//! Unix only. On any other platform this prints why and exits 1:
//! there is no signal to trap.

#![forbid(unsafe_code)]

#[cfg(unix)]
fn main() {
    unix::run();
}

#[cfg(not(unix))]
fn main() {
    eprintln!("stubborn needs a Unix signal mask; nothing to trap here");
    std::process::exit(1);
}

#[cfg(unix)]
mod unix {
    use std::time::Duration;

    use nix::sys::signal::{SigSet, Signal};

    /// How often the heartbeat prints while `SIGTERM` sits blocked
    /// and pending. Lets a person watching the log see the process
    /// is alive, not merely silent.
    const HEARTBEAT: Duration = Duration::from_secs(2);

    /// Blocks `SIGTERM` on the whole process. A mask set before any
    /// other thread spawns is inherited by every later thread.
    /// Starts the heartbeat, then loops forever reporting every
    /// `SIGTERM` it receives without exiting.
    ///
    /// Blocking (`thread_block`), not installing a handler
    /// (`sigaction`), keeps this file free of `unsafe`.
    /// `sigaction`/`signal` are unsafe in `nix`;
    /// `pthread_sigmask`/`sigwait` are not. A blocked signal is never
    /// delivered and stays pending, which is what "refuses to die"
    /// means.
    pub fn run() {
        println!(
            "stubborn pid={} ignoring SIGTERM; stop it with SIGKILL to end it for real",
            std::process::id()
        );

        let mut term = SigSet::empty();
        term.add(Signal::SIGTERM);
        term.thread_block().expect("SIGTERM must be blockable");

        std::thread::spawn(heartbeat);

        loop {
            // `sigwait` clears the pending flag on return, so a
            // second SIGTERM is reported again, not swallowed.
            term.wait().expect("sigwait must not fail");
            println!("stubborn: caught SIGTERM, still not dying");
        }
    }

    /// Prints a line every [`HEARTBEAT`]. Lets a person watching the
    /// log see the process is alive, not merely silent between
    /// deliveries.
    fn heartbeat() {
        loop {
            std::thread::sleep(HEARTBEAT);
            println!("stubborn: still here");
        }
    }
}
