//! The shepherd channel's message shapes, re-exported from shep-core.
//!
//! These types are defined in the `shep-channel` crate and reach here
//! through shep-core's `protocol` module, which re-exports them because
//! [`BusEvent::Channel`](shep_core::protocol::BusEvent) began carrying a
//! [`ChildMessage`] to bus subscribers: the event is a shep-core type, so the
//! message had to become reachable from shep-core too. This module stays as
//! the name shep-daemon knows them by inside `crate::channel` —
//! `ChildMessage`/`ShepherdMessage` are written across the runner, the
//! supervisor, the scripted fake and `tests/real_runner.rs`, and a re-export
//! keeps every one of those spellings correct.
//!
//! Nothing is defined here. Add a variant, a field or a fixture in
//! shep-channel.

pub use shep_core::protocol::{CHANNEL_VERSION, ChildMessage, ShepherdMessage};
