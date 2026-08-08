//! Async client for the shep daemon: connect-or-spawn state machine, typed
//! wrappers for every RPC verb, and event-bus subscription streams. This is
//! the programmatic API embedders use; the CLI is a thin consumer of it.
//!
//! Re-exports [`shep_core`] so downstream users need a single dependency.
//!
//! # Quick start
//! ```
//! use shep_client::shep_core::prelude::MemSize;
//!
//! let limit: MemSize = "512M".parse().unwrap();
//! assert_eq!(limit.bytes(), 512 << 20);
//! ```
//!
//! Module-by-module design: `docs/systematic-refactor/refactor-workspace/map.md`.

#![doc(test(attr(deny(warnings))))]
#![forbid(unsafe_code)]

// Unix-only: built on `tokio::net::UnixStream`, and — via `spawn` — on the
// exit-code contract of a `shep daemon` child. Gated at the `mod` line so
// each module's own `#[cfg(test)]` block goes with it; an inner
// `#![cfg(unix)]` would leave the declaration visible and the tests behind.
// Platform tiering follows shep-daemon's ruling — see the phase-3 plan's
// Global Constraints and `shep-daemon/Cargo.toml:34-40`.
#[cfg(unix)]
mod connection;
#[cfg(unix)]
pub use connection::{ConnectError, HANDSHAKE_TIMEOUT};

// Unix-only for the same reason as `connection` above: every fake here
// binds a `UnixListener`.
#[cfg(all(unix, any(test, feature = "test-support")))]
pub mod testing;

pub use shep_core;
