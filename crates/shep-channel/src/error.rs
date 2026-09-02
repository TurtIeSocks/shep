/// Anything that can go wrong on the shepherd channel.
///
/// `#[non_exhaustive]` per IR-20: this is on the peer-facing surface and the
/// channel will grow reasons to fail.
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
    /// [`crate::Channel::open`] call returns this instead of taking the
    /// inherited descriptor a second time, which would produce two values
    /// that both believe they own it.
    AlreadyTaken,
}

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

impl core::error::Error for ChannelError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}
