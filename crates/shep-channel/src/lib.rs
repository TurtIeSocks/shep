//! Speak the shep shepherd channel: signal readiness, emit a metric, answer
//! an action.
//!
//! An app supervised by shep can be handed a descriptor carrying
//! newline-delimited JSON in both directions. This crate finds that
//! descriptor, frames the JSON, and answers the messages an app does not
//! handle itself. `docs/shepherd-channel.md` in the shep repository is the
//! contract this implements.
//!
//! Doing nothing is the normal case: an app whose operator never asked for a
//! channel gets a handle whose every call is a no-op.

#![doc(test(attr(deny(warnings))))]
// Not `forbid`: taking the inherited descriptor needs one `unsafe` block,
// because the standard library has no safe constructor from a raw
// descriptor. That single site carries its own `// SAFETY:` and the
// workspace denies `undocumented_unsafe_blocks`, so it cannot lose it.
#![deny(unsafe_code)]

#[cfg(feature = "client")]
mod endpoint;
#[cfg(feature = "client")]
mod outbox;
#[cfg(feature = "client")]
mod session;
mod wire;

#[cfg(feature = "client")]
pub use endpoint::{Endpoint, FD_VAR, PIPE_VAR, VERSION_VAR};
pub use wire::{CHANNEL_VERSION, ChildMessage, ShepherdMessage};

/// Anything that can go wrong on the shepherd channel.
///
/// `#[non_exhaustive]` per IR-20: this is on the peer-facing surface and the
/// channel will grow reasons to fail.
#[cfg(feature = "client")]
#[non_exhaustive]
#[derive(Debug)]
pub enum ChannelError {
    /// The transport failed. Carries the underlying error.
    Io(std::io::Error),
    /// One frame could not be encoded or decoded. Carries serde's message.
    ///
    /// Recoverable: the frame is lost and the next call resumes at the next
    /// line, which is what the daemon does with a bad frame in the other
    /// direction.
    Malformed(String),
    /// The environment names a channel this platform cannot open, for
    /// example `SHEP_CHANNEL_PIPE` on unix. Carries the variable and value.
    Unusable(String),
    /// The writer has stopped and the message was not queued.
    Closed,
    /// This process already took its shepherd channel. A second
    /// [`Channel::open`] call returns this instead of taking the inherited
    /// descriptor a second time, which would produce two values that both
    /// believe they own it.
    AlreadyTaken,
}

#[cfg(feature = "client")]
impl core::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "shepherd channel I/O failed: {error}"),
            Self::Malformed(message) => write!(f, "malformed shepherd-channel frame: {message}"),
            Self::Unusable(what) => write!(f, "unusable shepherd channel: {what}"),
            Self::Closed => f.write_str("the shepherd channel is closed"),
            Self::AlreadyTaken => f.write_str(
                "the shepherd channel has already been taken by this process and can only be taken once",
            ),
        }
    }
}

#[cfg(feature = "client")]
impl core::error::Error for ChannelError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

/// The channel with no threads: you own the loop.
///
/// `serve` is the other road and the documented default, because it
/// answers the messages you did not register a handler for. Reach for this
/// when your app already has an event loop and wants the channel inside it.
#[cfg(feature = "client")]
#[derive(Debug)]
pub struct Channel {
    reader: std::io::BufReader<endpoint::Transport>,
    writer: endpoint::Transport,
    version: Option<String>,
}

#[cfg(feature = "client")]
impl Channel {
    /// Opens this process's channel, or `Ok(None)` when it has none.
    ///
    /// At most one channel exists per process. A second call does not take
    /// the inherited descriptor again -- which would produce two values
    /// that both believe they own it -- it returns
    /// [`ChannelError::AlreadyTaken`] instead.
    ///
    /// # Errors
    ///
    /// - [`ChannelError::Unusable`] when the environment names a channel
    ///   that cannot be opened here.
    /// - [`ChannelError::Io`] when the transport cannot be opened.
    /// - [`ChannelError::AlreadyTaken`] when this process already took its
    ///   channel.
    pub fn open() -> Result<Option<Self>, ChannelError> {
        let found = endpoint::discover()?;
        if found == endpoint::Endpoint::Absent {
            return Ok(None);
        }
        let (reader, writer) = endpoint::connect(&found)?;
        Ok(Some(Self {
            reader: std::io::BufReader::new(reader),
            writer,
            version: std::env::var(endpoint::VERSION_VAR).ok(),
        }))
    }

    /// Reads one message. `Ok(None)` is the shepherd closing its end.
    ///
    /// # Errors
    ///
    /// - [`ChannelError::Malformed`] for one unparseable line. Recoverable:
    ///   call again to resume at the next line.
    /// - [`ChannelError::Io`] when the transport fails.
    pub fn recv(&mut self) -> Result<Option<ShepherdMessage>, ChannelError> {
        session::read_message(&mut self.reader)
    }

    /// Writes one message and flushes it.
    ///
    /// # Errors
    ///
    /// - [`ChannelError::Io`] when the transport fails.
    /// - [`ChannelError::Malformed`] when the message cannot be encoded.
    pub fn send(&mut self, message: &ChildMessage) -> Result<(), ChannelError> {
        session::write_message(&mut self.writer, message)
    }

    /// The `SHEP_CHANNEL_VERSION` stamp, when the shepherd set one.
    ///
    /// A stamp, not a negotiation: the shepherd cannot ask what this app
    /// speaks. It is here so an app can notice a wire it has never seen.
    #[must_use]
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }
}
