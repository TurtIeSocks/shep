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
pub use client::{Client, DEADLINE_GRACE, DEFAULT_DEADLINE, RequestError, START_DEADLINE};
#[cfg(unix)]
pub use connection::{ConnectError, HANDSHAKE_TIMEOUT};
#[cfg(unix)]
pub use events::{EventStream, Lagged};
/// The trait [`EventStream`] implements.
///
/// Re-exported (IR-32: third-party re-exports normalized under our
/// namespace) because there is no stable `core::stream::Stream` — this
/// trait is otherwise unnameable in a caller's own bound without a direct
/// `futures-util` dependency. Pulling one event at a time needs no import
/// at all: [`EventStream::next`] is an inherent method. Only the trait
/// itself is re-exported, not `StreamExt`'s combinators — the inherent
/// `next` covers the common case, and a narrower surface is easier to widen
/// later than to walk back.
#[cfg(unix)]
#[doc(inline)]
pub use futures_util::Stream;

// Unix-only for the same reason as `connection` above: every fake here
// binds a `UnixListener`.
#[cfg(all(unix, any(test, feature = "test-support")))]
pub mod testing;

pub use shep_core;
