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
mod dispatch;
#[cfg(feature = "client")]
mod endpoint;
#[cfg(feature = "client")]
mod outbox;
#[cfg(feature = "client")]
mod session;
mod wire;

#[cfg(feature = "client")]
pub use dispatch::{ActionHandler, ShutdownHandler};
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
/// [`serve`] is the other road and the documented default, because it
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

#[cfg(feature = "client")]
use std::sync::atomic::{AtomicU32, Ordering};
#[cfg(feature = "client")]
use std::sync::{Arc, OnceLock, RwLock};

#[cfg(feature = "client")]
use crate::dispatch::{Dispatch, Outcome};
#[cfg(feature = "client")]
use crate::outbox::{DEFAULT_CAPACITY, Outbox};

/// What to tell an author running under shep with no channel.
#[cfg(feature = "client")]
const NO_CHANNEL_ADVICE: &str = "no channel on this process. Set `channel = true` \
     (or `wait_ready` / `shutdown_with_message`) on this app in the Flockfile to open one.";

/// What to tell an author whose app was asked to stop and registered nothing.
#[cfg(feature = "client")]
const UNHANDLED_SHUTDOWN_ADVICE: &str = "the shepherd sent shutdown and no on_shutdown handler is registered. This \
     process will be killed when kill_timeout expires. Register one to stop gracefully.";

/// Writes one line of advice to stderr, prefixed so it is attributable.
///
/// stderr rather than a log crate: this crate has no logging dependency and
/// an app's stderr is already where shep collects its bleats, so the author
/// reads this where they are already looking.
#[cfg(feature = "client")]
fn warn(message: &str) {
    eprintln!("shep-channel: {message}");
}

/// A handle on this process's shepherd channel.
///
/// Cheap to clone and safe to share: every method takes `&self`, so a
/// long-lived clone can sit in application state and emit from any thread.
/// With no channel, every method is a no-op, so nothing above this needs to
/// know whether the operator opted in.
#[cfg(feature = "client")]
#[derive(Clone, Debug)]
pub struct Shepherd(Arc<Inner>);

// Pins the "safe to share... from any thread" claim above at compile time,
// so a later field addition that quietly breaks it fails the build instead
// of the next reader's assumption.
#[cfg(feature = "client")]
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Shepherd>();
};

#[cfg(feature = "client")]
#[derive(Debug)]
struct Inner {
    /// `None` when this process has no channel.
    outbox: Option<Arc<Outbox>>,
    dispatch: Arc<RwLock<Dispatch>>,
    version: Option<String>,
}

#[cfg(feature = "client")]
impl Shepherd {
    fn inert(version: Option<String>) -> Self {
        Self(Arc::new(Inner {
            outbox: None,
            dispatch: Arc::new(RwLock::new(Dispatch::default())),
            version,
        }))
    }

    /// Whether this process actually has a channel.
    ///
    /// Branching on this is optional: every method already does nothing
    /// without one.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.0.outbox.is_some()
    }

    /// The `SHEP_CHANNEL_VERSION` stamp, when the shepherd set one.
    #[must_use]
    pub fn version(&self) -> Option<&str> {
        self.0.version.as_deref()
    }

    /// How many metrics have been dropped because the shepherd was not
    /// keeping up. Always 0 without a channel.
    #[must_use]
    pub fn dropped_metrics(&self) -> u64 {
        self.0.outbox.as_ref().map_or(0, |outbox| outbox.dropped())
    }

    /// Registers a handler for one action name, replacing any handler
    /// already registered under it.
    ///
    /// The handler is called with the action's params (`None` when the
    /// operator triggered it with none) and then the action's own name,
    /// and returns the reply body sent back to the operator.
    ///
    /// Registering after [`serve`] has started is fine and takes effect on
    /// the next message.
    pub fn on_action<H>(&self, name: impl AsRef<str>, handler: H) -> &Self
    where
        H: Fn(Option<&str>, &str) -> String + Send + Sync + 'static,
    {
        self.0
            .dispatch
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .register_action(name.as_ref().to_string(), Box::new(handler));
        self
    }

    /// Registers the handler run when the shepherd asks this app to stop.
    ///
    /// Without one, a shutdown message warns and nothing else happens: this
    /// crate never ends a process on its own judgement.
    pub fn on_shutdown<H>(&self, handler: H) -> &Self
    where
        H: Fn() + Send + Sync + 'static,
    {
        self.0
            .dispatch
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .register_shutdown(Box::new(handler));
        self
    }

    /// Says this app is up. Blocks only until the message is queued.
    ///
    /// # Errors
    ///
    /// [`ChannelError::Closed`] when the shepherd has gone away. Without a
    /// channel this is `Ok(())`, because an app that was never given one
    /// has nothing to report and no failure to handle.
    pub fn ready(&self) -> Result<(), ChannelError> {
        match &self.0.outbox {
            Some(outbox) => outbox.push_blocking(ChildMessage::Ready),
            None => Ok(()),
        }
    }

    /// Records one metric sample. Never blocks and never fails.
    ///
    /// A sample may be dropped if the shepherd stops reading; see
    /// [`Shepherd::dropped_metrics`]. That trade is deliberate, so that no
    /// call on an app's hot path can park on a full socket.
    ///
    /// Takes `impl Into<String>` rather than `on_action`'s `impl AsRef<str>`:
    /// this runs per sample on a documented hot path, and an owned `String`
    /// the caller already has moves in for free instead of being copied
    /// again.
    pub fn metric(&self, name: impl Into<String>, value: f64) {
        if let Some(outbox) = &self.0.outbox {
            outbox.push_lossy(ChildMessage::Metric {
                name: name.into(),
                value,
            });
        }
    }
}

/// Opens this process's channel and starts serving it.
///
/// Always returns a usable handle. With no channel every call on it is a
/// no-op, so an app needs no branch at its emit sites; one line goes to
/// stderr in that case, and only when `SHEP_NAME` says this process is
/// running under shep at all.
///
/// A process singleton: the channel is one descriptor and can be owned once.
/// A second call returns the same handle.
#[cfg(feature = "client")]
#[must_use]
pub fn serve() -> Shepherd {
    static SHEPHERD: OnceLock<Shepherd> = OnceLock::new();
    static CALLS: AtomicU32 = AtomicU32::new(0);

    let shepherd = SHEPHERD.get_or_init(start);
    if CALLS.fetch_add(1, Ordering::Relaxed) == 1 {
        warn(
            "serve() called more than once; returning the first handle. \
              The channel is one descriptor and cannot be opened twice.",
        );
    }
    shepherd.clone()
}

#[cfg(feature = "client")]
fn start() -> Shepherd {
    let channel = match Channel::open() {
        Ok(Some(channel)) => channel,
        Ok(None) => {
            if std::env::var_os("SHEP_NAME").is_some() {
                warn(NO_CHANNEL_ADVICE);
            }
            return Shepherd::inert(None);
        }
        Err(error) => {
            warn(&format!("{error}; continuing without a channel"));
            return Shepherd::inert(None);
        }
    };

    let (reader, mut writer, version) = channel.into_halves();
    if let Some(stamp) = &version
        && stamp != CHANNEL_VERSION
    {
        warn(&format!(
            "the shepherd stamps {VERSION_VAR}={stamp} and this crate implements \
             {CHANNEL_VERSION}; continuing, since a newer wire has so far only added \
             fields an older reader ignores"
        ));
    }

    let outbox = Arc::new(Outbox::new(DEFAULT_CAPACITY));
    let dispatch = Arc::new(RwLock::new(Dispatch::default()));

    let writing = Arc::clone(&outbox);
    let writer_spawn = std::thread::Builder::new()
        .name("shep-channel-writer".to_string())
        .spawn(move || {
            while let Some(message) = writing.pop() {
                if session::write_message(&mut writer, &message).is_err() {
                    break;
                }
            }
            writing.close();
        });
    if let Err(error) = writer_spawn {
        // Nothing will ever drain the outbox without this thread, so a
        // handle that reported `is_active()` true here would be worse than
        // no handle: `ready()` would queue into a channel nothing reads and
        // return `Ok(())`, and the operator's `wait_ready` gate would time
        // out with nothing anywhere saying why. Close the outbox first so
        // `ready()`/`metric()` see the failure honestly, then hand back an
        // inert handle -- still carrying the version stamp, since the
        // channel itself did open.
        warn(&format!(
            "failed to spawn the shep-channel writer thread: {error}; continuing without a channel"
        ));
        outbox.close();
        return Shepherd::inert(version);
    }

    let reading = Arc::clone(&outbox);
    let handlers = Arc::clone(&dispatch);
    let reader_spawn = std::thread::Builder::new()
        .name("shep-channel-reader".to_string())
        .spawn(move || {
            let mut reader = reader;
            let mut warned_malformed = false;
            loop {
                match session::read_message(&mut reader) {
                    Ok(Some(message)) => {
                        let outcome = handlers
                            .read()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .handle(message);
                        match outcome {
                            Outcome::Reply(reply) => {
                                if reading.push_blocking(reply).is_err() {
                                    break;
                                }
                            }
                            Outcome::Handled => {}
                            Outcome::UnhandledShutdown => warn(UNHANDLED_SHUTDOWN_ADVICE),
                            Outcome::ShutdownFailed(message) => {
                                warn(&format!("shutdown handler panicked: {message}"));
                            }
                        }
                    }
                    Err(ChannelError::Malformed(message)) => {
                        if !warned_malformed {
                            warned_malformed = true;
                            warn(&format!("malformed frame from the shepherd: {message}"));
                        }
                    }
                    Ok(None) | Err(_) => break,
                }
            }
            reading.close();
        });
    if let Err(error) = reader_spawn {
        // The writer is still useful without this thread: `ready()` and
        // `metric()` still reach the shepherd. Only actions go unanswered,
        // since nothing is left to read `ShepherdMessage::Action` off the
        // wire and dispatch it -- warn and hand back a handle that still
        // does the two things it can, rather than tearing the writer down
        // over a failure that does not touch it.
        warn(&format!(
            "failed to spawn the shep-channel reader thread: {error}; readiness and metrics still work, but no action sent to this process will ever be answered"
        ));
    }

    Shepherd(Arc::new(Inner {
        outbox: Some(outbox),
        dispatch,
        version,
    }))
}

#[cfg(all(test, feature = "client"))]
mod tests {
    use super::*;

    /// fails if a handle with no channel refuses work. An app must be able
    /// to call every method without asking whether it has a channel, which
    /// is the whole of D3.
    #[test]
    fn an_inert_handle_accepts_everything_and_does_nothing() {
        let shepherd = Shepherd::inert(None);
        assert!(!shepherd.is_active());
        shepherd.on_action("gc", |_, _| "ok".to_string());
        shepherd.on_shutdown(|| {});
        shepherd.metric("rps", 42.0);
        shepherd.ready().expect("an inert ready is not an error");
        assert_eq!(shepherd.dropped_metrics(), 0);
        assert_eq!(shepherd.version(), None);
    }

    /// fails if the no-channel advice stops naming all three fields that
    /// would open one. An author reading this line is deciding which to
    /// set. Asserts the exact `channel = true` clause, not the bare
    /// substring "channel" -- that substring also occurs in the advice's
    /// leading "no channel on this process" and would pass even with the
    /// whole `channel = true` clause deleted.
    #[test]
    fn the_no_channel_advice_names_every_field_that_opens_one() {
        for field in ["channel = true", "wait_ready", "shutdown_with_message"] {
            assert!(
                NO_CHANNEL_ADVICE.contains(field),
                "advice does not mention {field}"
            );
        }
    }

    /// fails if the shutdown warning stops naming the method an author has
    /// to call. D5 makes this warning the only thing between a missing
    /// handler and a `kill_timeout`.
    #[test]
    fn the_unhandled_shutdown_warning_names_the_method_to_call() {
        assert!(UNHANDLED_SHUTDOWN_ADVICE.contains("on_shutdown"));
        assert!(UNHANDLED_SHUTDOWN_ADVICE.contains("kill_timeout"));
    }
}
