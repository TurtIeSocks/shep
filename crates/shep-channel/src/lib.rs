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
//!
//! ```
//! let shepherd = shep_channel::serve();
//!
//! shepherd.on_action("gc", |params, _name| {
//!     format!("collected, params={params:?}")
//! });
//! shepherd.on_shutdown(|| { /* stop gracefully */ });
//!
//! // This doctest runs as a plain test process, not under shep, so the
//! // handle above has no channel: the two registrations above just sat
//! // down in an empty registry nobody will read, and the calls below are
//! // no-ops. That is the normal case, not a failure -- see `is_active`.
//! assert!(!shepherd.is_active());
//! shepherd.metric("rps", 4200.0);
//! shepherd.ready().unwrap();
//! ```
//!
//! Three things about that design are easy to miss:
//!
//! - The reader thread runs handlers itself, so a slow handler delays the
//!   next message behind it. `action_timeout` (set per app in the
//!   Flockfile) defaults to 3 seconds -- a handler that regularly takes
//!   longer than that is racing the shepherd's own patience, not just its
//!   caller's.
//! - `metric` can drop a sample under backpressure. `ready` and an action
//!   reply cannot: on a full queue they wait for room instead, because
//!   losing either silently is worse than a call that blocks. The queue is
//!   bounded for all three; what differs is who gives way.
//! - A shutdown message with no `on_shutdown` handler registered warns on
//!   stderr and does nothing else. This crate never stops a process on its
//!   own judgement; only a handler you registered does.

#![doc(test(attr(deny(warnings))))]
// Not `forbid`: taking the inherited descriptor needs one `unsafe` block,
// because the standard library has no safe constructor from a raw
// descriptor. That single site carries its own `// SAFETY:` and the
// workspace denies `undocumented_unsafe_blocks`, so it cannot lose it.
#![deny(unsafe_code)]

#[cfg(feature = "client")]
mod channel;
#[cfg(feature = "client")]
mod dispatch;
#[cfg(feature = "client")]
mod endpoint;
#[cfg(feature = "client")]
mod error;
#[cfg(feature = "client")]
mod outbox;
#[cfg(feature = "client")]
mod serve;
#[cfg(feature = "client")]
mod session;
mod wire;

#[cfg(feature = "client")]
pub use channel::Channel;
#[cfg(feature = "client")]
pub use dispatch::{ActionHandler, ShutdownHandler};
#[cfg(feature = "client")]
pub use endpoint::{Endpoint, FD_VAR, PIPE_VAR, VERSION_VAR, discover};
#[cfg(feature = "client")]
pub use error::ChannelError;
#[cfg(feature = "client")]
pub use serve::{Shepherd, serve};
pub use wire::{CHANNEL_VERSION, ChildMessage, ShepherdMessage};
