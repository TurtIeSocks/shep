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
mod actor;
#[cfg(unix)]
mod client;
#[cfg(unix)]
mod connection;
#[cfg(unix)]
mod events;
// `spawn` stays a public module rather than a flattened re-export: the
// exit-code contract (`spawn::DAEMON_ALREADY_RUNNING`) reads better
// qualified, a cross-crate agreement rather than a convenience import.
// Deliberately a plain `//` comment, not `///` — an outer doc comment on a
// `mod` declaration merges with the module file's own `//!` docs and
// resolves in crate-root scope, breaking that module's intra-doc links to
// its own siblings.
#[cfg(unix)]
pub mod spawn;
#[cfg(unix)]
pub use actor::EVENT_CHANNEL_CAPACITY;
#[cfg(unix)]
pub use client::{Client, DEADLINE_GRACE, DEFAULT_DEADLINE, RequestError, START_DEADLINE};
#[cfg(unix)]
pub use connection::{ConnectError, HANDSHAKE_TIMEOUT};
#[cfg(unix)]
pub use events::{EventStream, Lagged};

// The hand-rolled daemon fakes live in the `shep-client-testing` crate, not
// here: test scaffolding does not belong in this crate's published source,
// and a `test-support` feature could be switched on by a production consumer.
// That crate depends on this one and this one dev-depends on it — a cycle
// Cargo permits precisely because it runs through dev-dependencies.

pub use shep_core;
