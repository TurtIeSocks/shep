//! Hand-rolled daemon fakes for exercising [`shep_client`] — the ONE home for
//! every scripted daemon/client double: no shep-client module grows a second
//! `fake_daemon`, and no consumer crate defines its own.
//!
//! Its own crate rather than a module behind a `test-support` feature: test
//! scaffolding has no business in the published library's source, and a
//! feature flag can be switched on by a production consumer. This crate is
//! `publish = false`, which makes that structural rather than a matter of
//! discipline.
//!
//! Every helper takes the socket path as `&Path` — this crate carries no
//! dev-dependencies, so the caller owns the `TempDir`.
//!
//! [`fake_daemon`], [`sample_ack`], and [`sample_info`] are the
//! handshake-only primitives. [`FakeDaemon`] and the `fake_client_*` helpers
//! connect a real [`Client`](shep_client::Client) against a scripted peer, for
//! testing the connection actor's request/reply routing and beyond.

#![doc(test(attr(deny(warnings))))]
#![forbid(unsafe_code)]

// Unix-only: every fake here binds a `UnixListener`, and the `Client` they
// hand back only exists on unix. Gated at the `mod` line rather than with a
// crate-root `#![cfg(unix)]`, matching shep-client's own tiering, so the crate
// keeps its docs (and its `missing_docs` clean bill) on Windows.
#[cfg(unix)]
mod fakes;

#[cfg(unix)]
pub use fakes::{
    FakeDaemon, child_exiting_with, fake_client_capturing_envelopes, fake_client_event_then_reply,
    fake_client_on, fake_client_out_of_order, fake_client_replying_err,
    fake_client_that_closes_after_handshake, fake_client_that_dies_mid_request,
    fake_client_that_never_replies, fake_client_with_ack, fake_client_with_push, fake_daemon,
    fast_opts, sample_ack, sample_info, start_fake_daemon_answering_on,
};
