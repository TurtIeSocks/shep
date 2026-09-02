//! Finding the channel this process was given, and opening it.
//!
//! Branch on which environment variable is present, never on the platform.
//! The daemon sets exactly one of them and never both: `SHEP_CHANNEL_FD` on
//! unix, `SHEP_CHANNEL_PIPE` on Windows. Neither means no channel was opened
//! for this process, which is the ordinary case.

use std::path::PathBuf;

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

/// Opens the endpoint, returning the transport and a clone for the writer.
///
/// # Errors
///
/// - [`ChannelError::Unusable`] when the endpoint names a mechanism this
///   platform does not have.
/// - [`ChannelError::Io`] when the pipe cannot be opened or the descriptor
///   cannot be cloned.
pub(crate) fn connect(endpoint: &Endpoint) -> Result<(Transport, Transport), ChannelError> {
    let transport = match endpoint {
        #[cfg(unix)]
        Endpoint::Descriptor(fd) => {
            use std::os::fd::FromRawFd as _;
            // SAFETY: the shepherd hands this process exactly one descriptor
            // for the channel and names its number in `SHEP_CHANNEL_FD`.
            // Nothing else in this crate touches that number, and `serve` is
            // a process singleton, so this constructor runs at most once per
            // descriptor and takes sole ownership of it. A number that is not
            // ours produces an error on first use rather than undefined
            // behaviour, because the standard library's socket calls check.
            #[allow(unsafe_code)]
            unsafe {
                Transport::from_raw_fd(*fd)
            }
        }
        #[cfg(windows)]
        Endpoint::Pipe(path) => std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(ChannelError::Io)?,
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
    let writer = transport.try_clone().map_err(ChannelError::Io)?;
    Ok((transport, writer))
}
