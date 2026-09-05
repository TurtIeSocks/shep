//! Re-exports the shepherd channel's message types from
//! [`shep_core::protocol::channel`] as `crate::channel`, the name the runner,
//! the supervisor and the scripted fake all use.
//!
//! Nothing is defined here. Add a variant, a field or a fixture in shep-core.

pub use shep_core::protocol::channel::{CHANNEL_VERSION, ChildMessage, ShepherdMessage};
