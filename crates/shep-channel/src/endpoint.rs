//! Finding the channel this process was given, and opening it.
//!
//! Branch on which environment variable is present, never on the platform.
//! The daemon sets exactly one of them and never both: `SHEP_CHANNEL_FD` on
//! unix, `SHEP_CHANNEL_PIPE` on Windows. Neither means no channel was opened
//! for this process, which is the ordinary case.

use std::path::PathBuf;

use core::sync::atomic::{AtomicBool, Ordering};

use crate::ChannelError;

/// The descriptor number variable, set on unix only.
pub const FD_VAR: &str = "SHEP_CHANNEL_FD";
/// The named pipe variable, set on Windows only.
pub const PIPE_VAR: &str = "SHEP_CHANNEL_PIPE";
/// The wire version stamp, set on both platforms whenever a channel exists.
pub const VERSION_VAR: &str = "SHEP_CHANNEL_VERSION";

/// Where this process's channel is, if it has one.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// An inherited descriptor number, from `SHEP_CHANNEL_FD`.
    Descriptor(i32),
    /// A named pipe path, from `SHEP_CHANNEL_PIPE`.
    Pipe(PathBuf),
    /// Neither variable is set. Not an error.
    Absent,
}

/// Reads the environment and says where the channel is.
///
/// # Errors
///
/// [`ChannelError::Unusable`] when `SHEP_CHANNEL_FD` is set to something
/// that is not a descriptor number. That is a broken environment rather
/// than an absent channel, and saying so beats silently doing nothing.
pub fn discover() -> Result<Endpoint, ChannelError> {
    if let Some(raw) = std::env::var_os(FD_VAR) {
        let text = raw.to_string_lossy().into_owned();
        return text
            .trim()
            .parse::<i32>()
            .map(Endpoint::Descriptor)
            .map_err(|_| ChannelError::Unusable(format!("{FD_VAR}={text}")));
    }
    if let Some(raw) = std::env::var_os(PIPE_VAR) {
        return Ok(Endpoint::Pipe(PathBuf::from(raw)));
    }
    Ok(Endpoint::Absent)
}

/// The duplex this platform carries the channel on.
#[cfg(unix)]
pub(crate) type Transport = std::os::unix::net::UnixStream;
/// The duplex this platform carries the channel on.
#[cfg(windows)]
pub(crate) type Transport = std::fs::File;

/// Guards the inherited channel from being taken twice in one process.
///
/// Consumed only by the branch that actually takes a channel. A refusal --
/// `Endpoint::Absent`, or a `Descriptor`/`Pipe` naming a mechanism this
/// platform does not have -- takes nothing and must not consume it, or a
/// later, legitimate call would be refused for no reason.
static CHANNEL_TAKEN: AtomicBool = AtomicBool::new(false);

/// Opens the endpoint, returning the transport and a clone for the writer.
///
/// # Errors
///
/// - [`ChannelError::Unusable`] when the endpoint names a mechanism this
///   platform does not have.
/// - [`ChannelError::Io`] when the pipe cannot be opened or the descriptor
///   cannot be cloned.
/// - [`ChannelError::AlreadyTaken`] when this process already took its
///   channel.
pub(crate) fn connect(endpoint: &Endpoint) -> Result<(Transport, Transport), ChannelError> {
    let transport = match endpoint {
        #[cfg(unix)]
        Endpoint::Descriptor(fd) => {
            if CHANNEL_TAKEN.swap(true, Ordering::SeqCst) {
                return Err(ChannelError::AlreadyTaken);
            }
            use std::os::fd::FromRawFd as _;
            // SAFETY: the shepherd hands this process one descriptor for the
            // channel and names its number in `SHEP_CHANNEL_FD`.
            // `CHANNEL_TAKEN`, swapped just above, makes this arm reachable
            // at most once per process, and nothing else in this crate
            // touches that number, so this call takes sole ownership of it.
            // A number that is not ours still produces an error on first
            // use rather than undefined behaviour, because the standard
            // library's socket calls check.
            #[allow(unsafe_code)]
            unsafe {
                Transport::from_raw_fd(*fd)
            }
        }
        #[cfg(windows)]
        Endpoint::Pipe(path) => {
            if CHANNEL_TAKEN.swap(true, Ordering::SeqCst) {
                return Err(ChannelError::AlreadyTaken);
            }
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .map_err(ChannelError::Io)?
        }
        #[cfg(unix)]
        Endpoint::Pipe(path) => {
            return Err(ChannelError::Unusable(format!(
                "{PIPE_VAR}={} names a Windows named pipe and this is not Windows",
                path.display()
            )));
        }
        #[cfg(windows)]
        Endpoint::Descriptor(fd) => {
            return Err(ChannelError::Unusable(format!(
                "{FD_VAR}={fd} names an inherited descriptor and Windows does not inherit one"
            )));
        }
        Endpoint::Absent => {
            return Err(ChannelError::Unusable(
                "no channel: neither variable is set".to_string(),
            ));
        }
    };
    // `CHANNEL_TAKEN` is not released if this fails: once `transport` above
    // has taken the descriptor, the channel is taken for the life of the
    // process no matter what `try_clone` does next. There is no way to
    // hand a raw descriptor back once `from_raw_fd` owns it.
    let writer = transport.try_clone().map_err(ChannelError::Io)?;
    Ok((transport, writer))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::fd::IntoRawFd as _;
    #[cfg(unix)]
    use std::os::unix::net::UnixStream;

    use super::*;

    /// The only test in this crate that calls `connect()`. `CHANNEL_TAKEN`
    /// is process-global and tests share one process, so a second test
    /// that called `connect()` would find the channel already taken by
    /// this one -- there is nowhere else to put that test.
    ///
    /// Asserts the transition, not just the second call's error: the first
    /// call must succeed in this same test, or a `connect()` that refused
    /// everything would pass this test for the wrong reason.
    #[cfg(unix)]
    #[test]
    fn a_descriptor_can_only_be_taken_once() {
        let (ours, _theirs) = UnixStream::pair().expect("socketpair");
        let fd = ours.into_raw_fd();
        let endpoint = Endpoint::Descriptor(fd);

        let first = connect(&endpoint);
        assert!(first.is_ok(), "first take should succeed: {first:?}");

        let second = connect(&endpoint);
        assert!(matches!(second, Err(ChannelError::AlreadyTaken)));

        // `first` stays bound (not `let _ = ...`) through both calls above,
        // so its transport keeps the descriptor open rather than closing it
        // out from under `second`.
        drop(first);
    }
}
