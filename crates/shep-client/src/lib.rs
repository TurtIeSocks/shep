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
pub use client::{
    Client, DEADLINE_GRACE, DEFAULT_DEADLINE, LOG_PLANE_DEADLINE, RequestError, START_DEADLINE,
    TRIGGER_DEADLINE,
};
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
///
/// # Example
///
/// The one thing this re-export is for — writing the bound — with no
/// `futures-util` in the caller's own manifest:
///
/// ```
/// use shep_client::{EventStream, Stream};
///
/// fn _pending_hint<S: Stream>(stream: &S) -> Option<usize> {
///     stream.size_hint().1
/// }
///
/// // And the type that bound exists to accept. A live one comes from
/// // `Client::subscribe`, which needs a daemon; naming it does not.
/// fn _accepts(events: &EventStream) -> Option<usize> {
///     _pending_hint(events)
/// }
/// ```
#[cfg(unix)]
#[doc(inline)]
pub use futures_util::Stream;

// Unix-only for the same reason as `connection` above: every fake here
// binds a `UnixListener`.
#[cfg(all(unix, any(test, feature = "test-support")))]
pub mod testing;

pub use shep_core;

#[cfg(test)]
mod tests {
    /// Not an assertion about behaviour — a line of output where a wrong
    /// number is otherwise read in silence.
    ///
    /// A bare `cargo test -p shep-client` runs this crate's lib tests and
    /// nothing else. All four integration binaries carry
    /// `required-features = ["test-support"]` (see `Cargo.toml`), and cargo
    /// skips a target whose required features are off without a line, a
    /// count, or a warning — so a per-crate run reports a fraction of this
    /// crate's cases and presents it as the whole. That is exactly how a
    /// coverage or blast-radius measurement of this crate goes wrong.
    ///
    /// This case is compiled only when the feature is off, so it appears in
    /// precisely the runs that are missing those binaries and in none of the
    /// runs that include them (`--all-features`, `--workspace`, or anything
    /// pulling shep-cli's dev-dependency).
    #[cfg(not(feature = "test-support"))]
    #[test]
    fn heads_up_four_integration_binaries_need_test_support_and_are_not_running() {
        assert!(
            !cfg!(feature = "test-support"),
            "compiled only when the feature is off"
        );
    }
}
