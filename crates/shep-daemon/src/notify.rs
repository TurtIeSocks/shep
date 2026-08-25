//! Readiness reporting to an init system that supervises this process
//! directly
//!
//! One datagram — the eight bytes `READY=1\n` — sent to whatever address
//! `$NOTIFY_SOCKET` names. That is the whole of the `sd_notify` protocol
//! shep speaks, and it is deliberately the whole of this module: nothing
//! here watches for a reply, keeps the socket, or reports anything else
//! about the daemon's state.
//!
//! **No dependency, and no unsafe.** Both address shapes systemd can hand a
//! service are reachable from `std` alone: a filesystem path through
//! [`UnixDatagram::send_to`], and an `@`-prefixed abstract name through
//! `std::os::linux::net::SocketAddrExt::from_abstract_name` plus
//! [`UnixDatagram::send_to_addr`], both stable since 1.70. A crate for this
//! would buy nothing and this one is `#![deny(unsafe_code)]`.
//!
//! (The abstract-namespace half is a plain code span rather than a link on
//! purpose: it exists only on Linux, so an intra-doc link to it fails the
//! docs gate on a macOS build — the same reason this crate's taxonomy uses
//! code spans for names rustdoc cannot resolve in every configuration.)
//!
//! ## What fires it, and when
//!
//! [`crate::boot::boot`] sends this as the last thing it does, after the
//! muster restore has finished and the control plane is assembled — see
//! [`crate::boot::BootOptions::notify_socket`], which carries the address
//! there. The ordering is the point: a unit that reports itself ready at
//! exec time describes a flock that is not up yet, and a restore that hangs
//! reads as a healthy service supervising nothing. Reporting after the
//! restore turns that same hang into a failed start, and lets anything
//! ordered after the unit rely on the apps actually existing.
//!
//! ## Absent is the ordinary case
//!
//! Nothing sets `$NOTIFY_SOCKET` on an interactive `shep start`, on the
//! daemon the CLI autostarts for itself, or in any test — and launchd has
//! no readiness protocol at all, so nothing sets it on macOS either.
//! [`notify_ready`] answers `Ok(false)` there rather than failing, and
//! `boot` never calls this module at all unless an address reached it.

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
/// `Ok(false)` means the variable is unset — the ordinary case for a daemon
/// the CLI autostarted, and for launchd, which has no readiness protocol.
///
/// **No caller in this workspace**, and said plainly rather than left to be
/// discovered: `boot` is handed an already-resolved address instead (see
/// [`NOTIFY_SOCKET_ENV`] for why the environment read lives in the CLI), so
/// what holds this open is a caller that wants both halves in one call and
/// has no `BootOptions` to put an address into. `sys::adopt_fd` and
/// `boot::READY_FD_ENV` are public on the same terms.
///
/// # Errors
/// - [`NotifyError::Unsupported`] — the address names the abstract
///   namespace on a platform without one.
/// - [`NotifyError::Io`] — the socket could not be opened, or the datagram
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
/// - [`NotifyError::Io`] — the name was not a valid abstract address, the
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
/// - [`NotifyError::Unsupported`] — always. Linux is the only platform with
///   an abstract socket namespace to reach.
#[cfg(not(target_os = "linux"))]
fn send_to_abstract(_name: &[u8]) -> Result<(), NotifyError> {
    Err(NotifyError::Unsupported)
}

/// Errors reporting readiness.
///
/// Wraps `io::Error` directly rather than stringifying it, so a caller keeps
/// the underlying OS diagnostic through [`core::error::Error::source`]; that
/// costs this enum `Clone`/`PartialEq`/`Eq` (IR-19's documented exception
/// for variants wrapping `io::Error`), which nothing here needs.
///
/// `#[non_exhaustive]`: today's two variants cover one readiness transport
/// (an `AF_UNIX` datagram, abstract or pathname) failing to open or send. A
/// future transport — a named pipe where abstract sockets don't exist, or a
/// launchd/upstart equivalent of `sd_notify` — would need its own variant,
/// distinct from [`Self::Unsupported`]'s specifically-a-namespace-mismatch
/// meaning, and shep-daemon is a published library an out-of-tree matcher
/// should not break for (IR-20).
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

    /// fails if the datagram never leaves, or leaves with the wrong bytes.
    /// systemd matches `READY=1` literally; `ready=1`, `READY=true`, or a
    /// missing newline all leave the unit hanging until TimeoutStartSec,
    /// and none of them is visible from inside this process.
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

    /// fails if a bad address is swallowed. A silent success here is a unit
    /// that hangs for ninety seconds and then reports a timeout with nothing
    /// in the journal to say why.
    #[test]
    fn an_address_nothing_is_listening_on_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nobody-here.sock");
        assert!(notify(path.as_os_str()).is_err());
    }

    /// fails if an `@` address is not routed to the abstract namespace: the
    /// datagram has to reach a socket bound to that abstract name, and a
    /// `notify` that fell through to the filesystem branch would instead
    /// send to a *relative* path beginning with a literal `@`, reaching
    /// nothing.
    ///
    /// Linux-only, because the namespace is. **This case does not run on a
    /// macOS development machine** — it is compiled and executed on the
    /// Linux CI leg, and the sibling below is what macOS can observe. Said
    /// plainly rather than left implied: a `cargo test` here reports the
    /// other half of this pair and nothing about this one.
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

    /// fails if an `@` address is quietly treated as a filesystem path on a
    /// platform with no abstract namespace. The alternative to refusing it
    /// is writing into a file whose name literally starts with `@`, which
    /// can succeed while reporting readiness to nobody — the exact outcome
    /// this module exists to prevent, and one nothing inside this process
    /// could see.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn an_abstract_address_is_refused_where_there_is_no_such_namespace() {
        let sent = notify(std::ffi::OsStr::new("@shep-notify-nowhere"));
        assert!(
            matches!(sent, Err(NotifyError::Unsupported)),
            "there is no abstract namespace on this platform: {sent:?}"
        );
    }

    /// fails if an unset `$NOTIFY_SOCKET` is treated as a fault. Every
    /// interactive run and every test in this workspace is that case; an
    /// error there would turn the ordinary boot into a warning at best.
    ///
    /// The variable is read, never set: `std::env::set_var` is `unsafe` in
    /// edition 2024 and this crate is `#![deny(unsafe_code)]`, so the
    /// present-variable half is covered through [`notify`] instead, and the
    /// address reaches [`crate::boot::boot`] as a value rather than through
    /// the environment.
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
