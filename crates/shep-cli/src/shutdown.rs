//! One "the OS asked us to stop" signal, on both platforms.
//!
//! [`Terminate`] is where the platform difference is spent, so callers
//! keep one shape: install once, `recv().await` in a `select!`.
//!
//! Unix listens for `SIGTERM` alone. Windows merges all five console
//! control events, since `CTRL_CLOSE_EVENT`, `CTRL_SHUTDOWN_EVENT` and
//! `CTRL_LOGOFF_EVENT` arrive on a closing console or a machine going
//! down, and a stop request must cover those too.
//!
//! Windows terminates the process a few seconds after the handler
//! returns, so a shutdown path here should not assume it has long.

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
    /// `Option<()>` rather than `()`, so the unix arm passes through
    /// `Signal::recv`'s own `None` unchanged, keeping every call site's
    /// `while ... .is_some()` unchanged too.
    ///
    /// # Cancellation safety
    /// Safe on both platforms: each underlying stream is documented
    /// cancel-safe, and the Windows arm's `select!` over five of them
    /// inherits that.
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

    /// fails if the listener cannot be installed on this platform. Does
    /// not try to deliver a signal: raising a real `SIGTERM` would take
    /// the test harness down with it.
    #[tokio::test]
    async fn a_terminate_listener_installs_on_this_platform() {
        let listener = Terminate::install().expect("installing a terminate listener must work");
        assert!(format!("{listener:?}").contains("Terminate"));
    }

    /// fails if the listener resolves without anything having been sent:
    /// a mis-wired `select!` branch would make every long-running verb
    /// exit the instant it started.
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
