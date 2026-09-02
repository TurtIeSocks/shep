use crate::{ChannelError, ChildMessage, ShepherdMessage, endpoint, session};

/// The channel with no threads: you own the loop.
///
/// [`crate::serve`] is the other road and the documented default, because it
/// answers the messages you did not register a handler for. Reach for this
/// when your app already has an event loop and wants the channel inside it.
#[derive(Debug)]
pub struct Channel {
    pub(crate) reader: std::io::BufReader<endpoint::Transport>,
    pub(crate) writer: endpoint::Transport,
    pub(crate) version: Option<String>,
}

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

    /// Takes the channel apart for the two threads that drive it.
    pub(crate) fn into_halves(
        self,
    ) -> (
        std::io::BufReader<endpoint::Transport>,
        endpoint::Transport,
        Option<String>,
    ) {
        (self.reader, self.writer, self.version)
    }
}
