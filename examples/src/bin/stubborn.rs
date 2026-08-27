//! Traps `SIGTERM` and refuses to die, so `kill_timeout` elapses and shep
//! escalates to `SIGKILL`. (`kill_timeout`, not `graceful_timeout` --
//! that field only governs a *reload*'s drain window for the old instance;
//! the grace period between a plain stop signal and SIGKILL is
//! `kill_timeout`, and that's what `examples/Flockfile.toml` sets below.)
//!
//! A survey of 131 real repositories behind this example found nothing that
//! does this on purpose, which is exactly why the escalation path has never
//! been watched by a person: every real app dies on the first signal, so
//! `kill_timeout`'s expiry and shep's follow-up `SIGKILL` only ever fire
//! in a test. Stop this one with `shep stop` and watch both happen for real.
//!
//! # Usage
//!
//! ```text
//! stubborn
//! ```
//!
//! Unix only. On any other platform this prints why and exits 1 — there is
//! no signal to trap.

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

    /// How often the heartbeat prints while `SIGTERM` sits blocked and
    /// pending, so a person watching the log can see the process is alive
    /// and simply not acting on the signal, rather than merely silent.
    const HEARTBEAT: Duration = Duration::from_secs(2);

    /// Blocks `SIGTERM` on the whole process (a mask set before any other
    /// thread is spawned is inherited by every thread that comes after),
    /// starts the heartbeat thread, then loops forever reporting every
    /// `SIGTERM` delivery it receives without ever exiting.
    ///
    /// Blocking the signal (`thread_block`), not installing a handler
    /// (`sigaction`), is what keeps this file free of `unsafe`:
    /// `sigaction`/`signal` are `unsafe` in `nix`, while
    /// `pthread_sigmask`/`sigwait` are not. A blocked signal is never
    /// delivered to anyone and stays pending, which is "refuses to die" —
    /// the process never even runs a handler that could call `exit`.
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
            // `sigwait` clears the pending flag on return, so a second
            // SIGTERM (shep's own retried stop signal, or an operator's
            // second `shep stop`) is reported again rather than swallowed.
            term.wait().expect("sigwait must not fail");
            println!("stubborn: caught SIGTERM, still not dying");
        }
    }

    /// Prints a line every [`HEARTBEAT`], so a person watching the log can
    /// see the process is alive and simply not acting on `SIGTERM`, rather
    /// than merely silent between deliveries.
    fn heartbeat() {
        loop {
            std::thread::sleep(HEARTBEAT);
            println!("stubborn: still here");
        }
    }
}
