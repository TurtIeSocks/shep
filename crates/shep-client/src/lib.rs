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

// the `DogConfig` derive expands to `impl ::shep_client::dogs::DogConfig`,
// a path this crate has no other way to name for its own tests
extern crate self as shep_client;

// portable: `connection` names `shep_core::transport::ClientStream`, not
// `tokio::net::UnixStream`, so the OS choice is made one crate down.
// `spawn`'s exit-code contract reads `ExitStatus::code()`, portable already.
mod actor;
mod client;
mod connection;
// public: a dog is a third-party binary, so this module is the API it's
// written against; kept as a module rather than flattened, like `spawn`
pub mod dogs;
mod events;
mod reconnect;
// public module, not a flattened re-export, so `spawn::DAEMON_ALREADY_RUNNING`
// reads as a qualified cross-crate contract. Plain `//`, not `///`: an outer
// doc on a `mod` item merges into the crate root and breaks this module's
// own intra-doc links.
pub mod spawn;
pub use client::{
    Client, DEADLINE_GRACE, DEFAULT_DEADLINE, LOG_PLANE_DEADLINE, RequestError, START_DEADLINE,
    TRIGGER_DEADLINE,
};
pub use connection::{ConnectError, HANDSHAKE_TIMEOUT};
pub use events::{EventStream, Lagged};
/// The trait [`EventStream`] implements.
///
/// Re-exported because there is no stable `core::stream::Stream`, and this
/// trait is otherwise unnameable in a caller's own bound without a direct
/// `futures-util` dependency. Pulling one event at a time needs no import:
/// [`EventStream::next`] is an inherent method. Only the trait itself is
/// re-exported, not `StreamExt`'s combinators.
///
/// # Example
///
/// Writing the bound with no `futures-util` in the caller's own manifest:
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
#[doc(inline)]
pub use futures_util::Stream;
pub use reconnect::{LinkState, RECONNECT_MAX_DELAY, RECONNECT_MIN_DELAY, ReconnectingClient};

// Portable for the same reason as `connection` above: every fake here binds
// a `shep_core::transport::Listener` rather than a `UnixListener`.
#[cfg(any(test, feature = "test-support"))]
pub mod testing;

pub use shep_core;

/// The wire protocol this client speaks, re-exported from
/// [`shep_core::protocol::PROTOCOL_VERSION`].
///
/// Reachable from this crate's own root because a dog depends on this
/// crate and nothing else, and `docs/dogs.md` asks every dog to print it.
///
/// A re-export rather than a copy: a second definition here could
/// disagree with the value the handshake actually compares.
pub use shep_core::protocol::PROTOCOL_VERSION;

#[cfg(test)]
mod tests {
    /// Not an assertion, just a line of output where a wrong number would
    /// otherwise pass in silence.
    ///
    /// The four integration binaries require `test-support` (`Cargo.toml`),
    /// and cargo skips a target whose required features are off without a
    /// line or a warning, so a bare `cargo test -p shep-client` silently
    /// reports a fraction of this crate's tests as the whole. This case
    /// compiles only when the feature is off, so it appears in exactly
    /// the runs missing those binaries.
    #[cfg(not(feature = "test-support"))]
    #[test]
    fn heads_up_four_integration_binaries_need_test_support_and_are_not_running() {
        assert!(
            !cfg!(feature = "test-support"),
            "compiled only when the feature is off"
        );
    }
}
