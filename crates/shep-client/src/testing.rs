//! Hand-rolled test doubles shared across this phase's tasks (and, via the
//! `test-support` feature, shep-cli's own tests).
//!
//! This is the ONE home for every fake used to test shep-client: no other
//! module grows a second `fake_daemon`, and no other crate defines its own.
//! Every helper takes the socket path as `&Path` — this module carries no
//! dev-dependencies (it compiles into an ordinary build under
//! `test-support`, so `missing_docs` and `missing_debug_implementations`
//! apply to it exactly like any other public module), so the caller owns
//! the `TempDir`.
//!
//! Roster grows task by task: this task (1) writes `fake_daemon`,
//! `sample_ack`, and `sample_info`; later tasks in this phase add
//! `FakeDaemon` and its many flavors as their own types come into
//! existence.

use std::path::Path;

use futures_util::{SinkExt, StreamExt};
use tokio::net::UnixListener;
use tokio::task::JoinHandle;
use tokio_util::codec::Framed;

use shep_core::protocol::{
    Hello, HelloAck, HelloReply, PROTOCOL_VERSION, ProcessInfo, codec, decode_frame, encode_frame,
};
use shep_core::status::ProcStatus;

/// Serves exactly one connection, replying to the `Hello` with `reply`,
/// then closing. The returned handle yields the `Hello` the client
/// actually sent, so a test can assert on the announcement as well as on
/// the answer.
///
/// Binds before returning, so a caller that awaits it can `connect`
/// immediately without a sleep.
///
/// # Panics
///
/// Panics if `path` cannot be bound, or if the served connection fails to
/// accept, read the `Hello` frame, decode it, or send `reply` back — this
/// is test scaffolding, meant to fail the test loudly rather than surface
/// a `Result` nobody would check.
pub async fn fake_daemon(path: &Path, reply: HelloReply) -> JoinHandle<Hello> {
    let listener = UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut frames = Framed::new(stream, codec());
        let first = frames.next().await.unwrap().unwrap();
        let hello: Hello = decode_frame(&first).unwrap();
        frames.send(encode_frame(&reply).unwrap()).await.unwrap();
        hello
    })
}

/// A `HelloAck` with a distinctive version and pid, so a test that asserts
/// on either can tell a real read from a default.
#[must_use]
pub fn sample_ack() -> HelloAck {
    HelloAck {
        daemon_version: "9.9.9".into(),
        protocol: PROTOCOL_VERSION,
        pid: 4242,
    }
}

/// One fully-populated [`ProcessInfo`] — every `Option` is `Some`, so a
/// payload type's anti-drift test sees every serialized field.
#[must_use]
pub fn sample_info() -> ProcessInfo {
    ProcessInfo {
        id: 1,
        name: "web".to_string(),
        status: ProcStatus::Online,
        pid: Some(4242),
        restarts: 3,
        uptime_ms: 60_000,
        fold: Some("backend".to_string()),
    }
}
