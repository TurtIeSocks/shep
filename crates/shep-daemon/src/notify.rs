//! Readiness reporting to an init system that supervises this process
//! directly.
//!
//! Sends the eight bytes `READY=1\n` to `$NOTIFY_SOCKET`: the whole of the
//! `sd_notify` protocol shep speaks. No dependency and no unsafe; both
//! address shapes systemd hands a service (a filesystem path, an
//! `@`-prefixed abstract name) are reachable from `std` alone since 1.70.
//!
//! [`crate::boot::boot`] sends this last, after the muster restore
//! finishes, so a unit reports ready only once the flock is up. Absent is
//! the ordinary case: nothing sets `$NOTIFY_SOCKET` outside a real systemd
//! unit, and [`notify_ready`] answers `Ok(false)` rather than failing.

use core::fmt;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixDatagram;
use std::path::Path;

/// The variable an init system sets on a service that reports its own
/// readiness.
///
/// Read by `shep-cli`'s hidden `daemon` subcommand, alongside every `SHEP_*`
/// override it already reads, and never inside this crate: a test cannot
/// establish an ambient value to observe against, because `std::env::set_var`
/// is `unsafe` in edition 2024 and both crates refuse unsafe. What crosses
/// the boundary is the resolved address, in
/// [`BootOptions::notify_socket`](crate::boot::BootOptions::notify_socket).
pub const NOTIFY_SOCKET_ENV: &str = "NOTIFY_SOCKET";

/// The one message this module sends.
///
/// Matched literally by systemd: `ready=1`, `READY=true`, or a missing
/// newline all leave the unit hanging until `TimeoutStartSec`, and none of
/// those is visible from inside this process.
const READY: &[u8] = b"READY=1\n";

/// The prefix that makes an address name the abstract socket namespace
/// rather than a filesystem path (systemd's own convention, standing in for
/// the leading NUL byte such an address really carries).
const ABSTRACT_PREFIX: u8 = b'@';

/// Sends `READY=1` to `$NOTIFY_SOCKET`, reporting whether there was one.
///
/// `Ok(false)` means the variable is unset: the ordinary case for a daemon
/// the CLI autostarted, and for launchd, which has no readiness protocol.
/// No caller in this workspace; `boot` uses an already-resolved address
/// instead (see [`NOTIFY_SOCKET_ENV`]).
///
/// # Errors
/// - [`NotifyError::Unsupported`]: the address names the abstract
///   namespace on a platform without one.
/// - [`NotifyError::Io`]: the socket could not be opened, or the datagram
///   not sent.
pub fn notify_ready() -> Result<bool, NotifyError> {
    match std::env::var_os(NOTIFY_SOCKET_ENV) {
        Some(target) => notify(&target).map(|()| true),
        None => Ok(false),
    }
}

/// Sends `READY=1` to one already-resolved address.
///
/// A leading `@` selects the abstract namespace; anything else is a
/// filesystem path. Refusing an `@` address where there is no such
/// namespace beats sending into a file literally named `@…`, which would
/// succeed silently and report readiness to nobody.
///
/// # Errors
/// As [`notify_ready`].
pub fn notify(target: &OsStr) -> Result<(), NotifyError> {
    match target.as_bytes().strip_prefix(&[ABSTRACT_PREFIX]) {
        Some(name) => send_to_abstract(name),
        None => {
            let socket = UnixDatagram::unbound()?;
            socket.send_to(READY, Path::new(target))?;
            Ok(())
        }
    }
}

/// Sends [`READY`] to the abstract-namespace address `name` (the address
/// without its `@`).
///
/// # Errors
/// - [`NotifyError::Io`]: the name was not a valid abstract address, the
///   socket could not be opened, or the datagram not sent.
#[cfg(target_os = "linux")]
fn send_to_abstract(name: &[u8]) -> Result<(), NotifyError> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::SocketAddr;

    let addr = SocketAddr::from_abstract_name(name)?;
    let socket = UnixDatagram::unbound()?;
    socket.send_to_addr(READY, &addr)?;
    Ok(())
}

/// Refuses an abstract-namespace address on a unix without one.
///
/// # Errors
/// - [`NotifyError::Unsupported`]: always. Linux is the only platform with
///   an abstract socket namespace to reach.
#[cfg(not(target_os = "linux"))]
fn send_to_abstract(_name: &[u8]) -> Result<(), NotifyError> {
    Err(NotifyError::Unsupported)
}

/// Errors reporting readiness.
///
/// Wraps `io::Error` directly rather than stringifying it, so a caller keeps
/// the underlying OS diagnostic through [`core::error::Error::source`]. That
/// costs this enum `Clone`, `PartialEq` and `Eq`, which nothing here needs.
///
/// `#[non_exhaustive]`: a future transport (a named pipe, a launchd
/// equivalent of `sd_notify`) would need its own variant, distinct from
/// [`Self::Unsupported`]'s namespace-mismatch meaning.
#[non_exhaustive]
#[derive(Debug)]
pub enum NotifyError {
    /// The address named the abstract socket namespace (a leading `@`) on a
    /// platform that has no such namespace.
    Unsupported,
    /// The datagram socket could not be opened, or the datagram not sent
    /// (carries the OS error).
    Io(std::io::Error),
}

impl fmt::Display for NotifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => write!(
                f,
                "an `@` address names the abstract socket namespace, which this platform has none of"
            ),
            Self::Io(err) => write!(f, "readiness could not be reported: {err}"),
        }
    }
}

impl core::error::Error for NotifyError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Unsupported => None,
            Self::Io(err) => Some(err),
        }
    }
}

impl From<std::io::Error> for NotifyError {
    fn from(source: std::io::Error) -> Self {
        Self::Io(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::time::Duration;

    #[test]
    fn ready_reaches_a_listening_socket_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify.sock");
        let listener = UnixDatagram::bind(&path).unwrap();
        // Bounded: an unread datagram would otherwise park this test
        // forever rather than failing it.
        listener
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        notify(path.as_os_str()).unwrap();

        let mut buf = [0u8; 64];
        let read = listener.recv(&mut buf).unwrap();
        assert_eq!(&buf[..read], b"READY=1\n");
    }

    #[test]
    fn an_address_nothing_is_listening_on_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nobody-here.sock");
        assert!(notify(path.as_os_str()).is_err());
    }

    /// Linux-only, because the namespace is: this runs on the Linux CI leg,
    /// and the sibling below is what macOS runs instead.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_abstract_address_reaches_the_abstract_namespace() {
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::net::SocketAddr;

        // The abstract namespace is kernel-wide rather than per-directory,
        // so the name carries this process's pid: two test binaries running
        // at once must not bind the same address.
        let name = format!("shep-notify-{}", std::process::id());
        let addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let listener = UnixDatagram::bind_addr(&addr).unwrap();
        // Bounded, for the reason the case above gives.
        listener
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        let mut target = std::ffi::OsString::from("@");
        target.push(&name);
        notify(&target).unwrap();

        let mut buf = [0u8; 64];
        let read = listener.recv(&mut buf).unwrap();
        assert_eq!(&buf[..read], b"READY=1\n");
    }

    /// The alternative to refusing it is writing into a file literally
    /// named `@shep-notify-nowhere`, which would succeed while reporting
    /// readiness to nobody.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn an_abstract_address_is_refused_where_there_is_no_such_namespace() {
        let sent = notify(std::ffi::OsStr::new("@shep-notify-nowhere"));
        assert!(
            matches!(sent, Err(NotifyError::Unsupported)),
            "there is no abstract namespace on this platform: {sent:?}"
        );
    }

    #[test]
    fn an_unset_notify_socket_reports_nothing_and_is_not_an_error() {
        if std::env::var_os(NOTIFY_SOCKET_ENV).is_some() {
            // Not a skip that hides a regression: it means this binary was
            // itself run as a notify-type service, where `Ok(true)` is the
            // correct answer and the case has nothing left to assert.
            return;
        }
        assert!(
            !notify_ready().unwrap(),
            "an unset $NOTIFY_SOCKET reports that nothing was told, not that something was"
        );
    }
}
