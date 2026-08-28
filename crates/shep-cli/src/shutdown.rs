//! One "the OS asked us to stop" signal, on both platforms
//!
//! Three long-running things in this crate — the `lookout` dashboard, the
//! bark dog and the metrics dog — each installed a `SIGTERM` listener of
//! their own and selected on it. That is exactly right on unix and does not
//! compile on Windows, where there are no signals and the equivalent request
//! arrives as one of five distinct console control events.
//!
//! [`Terminate`] is the one place that difference is spent, so the three
//! call sites keep the shape they had: install once, `recv().await` in a
//! `select!`.
//!
//! # What "terminate" means per platform
//!
//! Unix listens for `SIGTERM` alone, unchanged — `SIGINT` is the terminal's
//! own Ctrl-C and these three already handle a key press or a closed stdin
//! separately.
//!
//! Windows merges all five console control events, and that is a wider net
//! than `SIGTERM` on purpose: `CTRL_C_EVENT` and `CTRL_BREAK_EVENT` are what
//! an operator sends by hand, while `CTRL_CLOSE_EVENT`,
//! `CTRL_SHUTDOWN_EVENT` and `CTRL_LOGOFF_EVENT` are what arrive when the
//! console window closes or the machine goes down. Treating only the first
//! two as a stop request would leave a dog running through a reboot with no
//! chance to flush.
//!
//! **The three close events carry a hard OS deadline** — Windows terminates
//! the process a few seconds after the handler returns, and nothing in the
//! process can extend it. So a shutdown path that must finish work should
//! not assume it has long. `shep_daemon::boot`'s own signal installer
//! carries the same caveat for the shepherd itself.

use std::io;

/// A listener for the OS's request to stop.
pub(crate) struct Terminate {
    #[cfg(unix)]
    signal: tokio::signal::unix::Signal,
    #[cfg(windows)]
    ctrl_c: tokio::signal::windows::CtrlC,
    #[cfg(windows)]
    ctrl_break: tokio::signal::windows::CtrlBreak,
    #[cfg(windows)]
    ctrl_close: tokio::signal::windows::CtrlClose,
    #[cfg(windows)]
    ctrl_shutdown: tokio::signal::windows::CtrlShutdown,
    #[cfg(windows)]
    ctrl_logoff: tokio::signal::windows::CtrlLogoff,
}

impl core::fmt::Debug for Terminate {
    /// Hand-written because none of the underlying signal types is `Debug`.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Terminate").finish_non_exhaustive()
    }
}

impl Terminate {
    /// Installs the listener.
    ///
    /// # Errors
    ///
    /// [`io::Error`] if the OS refuses to register a handler.
    pub(crate) fn install() -> io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                signal: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
            })
        }
        #[cfg(windows)]
        {
            use tokio::signal::windows;
            Ok(Self {
                ctrl_c: windows::ctrl_c()?,
                ctrl_break: windows::ctrl_break()?,
                ctrl_close: windows::ctrl_close()?,
                ctrl_shutdown: windows::ctrl_shutdown()?,
                ctrl_logoff: windows::ctrl_logoff()?,
            })
        }
    }

    /// Resolves when the OS asks this process to stop.
    ///
    /// `Option<()>` rather than `()` so the unix arm can pass through
    /// `Signal::recv`'s own `None` (the stream closed) unchanged, keeping
    /// the three call sites' `while ... .is_some()` and `select!` arms
    /// exactly as they were.
    ///
    /// # Cancellation safety
    ///
    /// Safe on both platforms, which every call site depends on — all three
    /// poll this inside a `select!` against other work, so a cancelled
    /// branch must not drop a pending signal. Each underlying stream is
    /// documented cancel-safe, and the Windows arm's `select!` over five of
    /// them inherits that: a cancellation drops the outer future without
    /// consuming any inner one's notification.
    pub(crate) async fn recv(&mut self) -> Option<()> {
        #[cfg(unix)]
        {
            self.signal.recv().await
        }
        #[cfg(windows)]
        {
            tokio::select! {
                received = self.ctrl_c.recv() => received,
                received = self.ctrl_break.recv() => received,
                received = self.ctrl_close.recv() => received,
                received = self.ctrl_shutdown.recv() => received,
                received = self.ctrl_logoff.recv() => received,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if the listener cannot be installed on this platform.
    ///
    /// Deliberately does not try to DELIVER a signal: raising a real
    /// `SIGTERM`, or generating a real console control event, would take the
    /// test harness's own process down with it. Installation is the half
    /// that differs per platform and the half that can break silently; that
    /// a delivered signal wakes the stream is tokio's contract, not this
    /// module's.
    #[tokio::test]
    async fn a_terminate_listener_installs_on_this_platform() {
        let listener = Terminate::install().expect("installing a terminate listener must work");
        // Also pins that `Debug` renders without needing the inner types to,
        // which is the reason it is hand-written.
        assert!(format!("{listener:?}").contains("Terminate"));
    }

    /// fails if the listener resolves without anything having been sent.
    ///
    /// The Windows arm is a `select!` over five streams, and a `select!`
    /// whose branches were mis-wired (a `recv()` that returns immediately,
    /// say) would make every long-running verb exit the instant it started.
    /// That is a silent, total failure, so it earns a test.
    #[tokio::test]
    async fn an_uninvoked_listener_does_not_resolve() {
        let mut listener = Terminate::install().unwrap();
        let early =
            tokio::time::timeout(std::time::Duration::from_millis(150), listener.recv()).await;
        assert!(
            early.is_err(),
            "recv() must park until the OS actually asks us to stop"
        );
    }
}
