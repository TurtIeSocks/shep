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
pub use endpoint::{Endpoint, FD_VAR, PIPE_VAR, VERSION_VAR};
#[cfg(feature = "client")]
pub use error::ChannelError;
#[cfg(feature = "client")]
pub use serve::{Shepherd, serve};
pub use wire::{CHANNEL_VERSION, ChildMessage, ShepherdMessage};
