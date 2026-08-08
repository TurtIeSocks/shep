//! `Client` request/reply routing, error surfacing and deadlines, driven
//! against the hand-rolled daemon fakes in `shep-client-testing`.
//!
//! An integration test rather than a `#[cfg(test)] mod tests` block inside
//! `client.rs`, because the fakes live in a crate that itself links
//! `shep-client`. A unit-test build compiles this lib a *second* time (with
//! `--cfg test`) as the test binary's root, so the `Client` a fake hands back
//! and the `Client` the module names would be two distinct types from two
//! distinct copies of the library — the compiler rejects it, and the copy the
//! fakes returned is not even the one under test. Linked as an ordinary
//! external crate here, there is exactly one `shep-client` and these tests
//! exercise the real one.

#![cfg(unix)]

use std::time::Duration;

use shep_client::{DEADLINE_GRACE, DEFAULT_DEADLINE, RequestError, START_DEADLINE};
use shep_client_testing::{
    fake_client_capturing_envelopes, fake_client_event_then_reply, fake_client_on,
    fake_client_out_of_order, fake_client_replying_err, fake_client_that_closes_after_handshake,
    fake_client_that_dies_mid_request, fake_client_that_never_replies,
};
use shep_core::protocol::{Request, Response, RpcErrorCode};

#[tokio::test]
async fn two_concurrent_requests_each_get_their_own_reply() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    // Fake daemon replies to id 1 with Pong and id 2 with Flock(vec![]) — DELIBERATELY
    // out of order (2 first) to prove routing is by id, not by arrival order.
    let (client, _served) = fake_client_out_of_order(&path).await;
    let (a, b) = tokio::join!(
        client.request(Request::Ping),
        client.request(Request::ListFlock),
    );
    assert!(matches!(a.unwrap(), Response::Pong));
    assert!(matches!(b.unwrap(), Response::Flock(f) if f.is_empty()));
}

#[tokio::test]
async fn an_event_arriving_before_its_own_reply_does_not_break_the_request() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    // Fake daemon sends BusEvent::Process{..} FIRST, then Reply{id:1}. This is the real
    // supervisor's ordering, not a contrived one — see daemon_e2e.rs:161-174.
    let (client, _served) = fake_client_event_then_reply(&path).await;
    assert!(matches!(
        client.request(Request::Ping).await.unwrap(),
        Response::Pong
    ));
}

#[tokio::test]
async fn a_daemon_side_error_reply_becomes_a_typed_rpc_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let (client, _served) =
        fake_client_replying_err(&path, RpcErrorCode::NotFound, "no sheep matched").await;
    let RequestError::Rpc(err) = client.request(Request::ListFlock).await.unwrap_err() else {
        panic!("an Err reply must surface as RequestError::Rpc");
    };
    assert_eq!(err.code, RpcErrorCode::NotFound);
}

#[tokio::test]
async fn a_dropped_connection_fails_pending_requests_instead_of_hanging() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let (client, served) = fake_client_that_closes_after_handshake(&path).await;
    served.await.unwrap();
    assert!(matches!(
        client.request(Request::Ping).await,
        Err(RequestError::Closed)
    ));
}

/// Unlike the test above, the request here is genuinely in the actor's
/// `pending` map when the connection dies — the fake daemon reads it (proving
/// the write already succeeded) before dropping the connection, rather than
/// dropping before any request is ever sent. This is the path `actor.rs`'s
/// drain-on-close loop exists for.
#[tokio::test]
async fn a_connection_dying_mid_request_fails_the_pending_request_instead_of_hanging() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let (client, _served) = fake_client_that_dies_mid_request(&path).await;
    assert!(matches!(
        client.request(Request::Ping).await,
        Err(RequestError::Closed)
    ));
}

#[tokio::test(start_paused = true)]
async fn a_deadline_expires_client_side_when_the_daemon_never_answers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let (client, _served) = fake_client_that_never_replies(&path).await;
    let err = client
        .request_with_deadline(Request::Ping, Some(Duration::from_millis(250)))
        .await
        .unwrap_err();
    let RequestError::Timeout { after } = err else {
        panic!("expected a client-side timeout, got {err:?}")
    };
    assert_eq!(after, Duration::from_millis(250) + DEADLINE_GRACE);
}

/// A `kill` that polled the wrong path would wait out its whole teardown
/// budget and report "still tearing down" against a daemon that shut down
/// cleanly — and no other test here reads `socket()` at all.
#[tokio::test]
async fn a_client_remembers_the_path_it_connected_through() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let (client, _served) = fake_client_on(&path).await;
    assert_eq!(client.socket(), path);
}

/// Nothing else here reads the envelope the client actually sent, so an
/// implementation that always sends `deadline_ms: None` would pass every test
/// above while silently inheriting the daemon's default for every verb. This
/// is the same gap the Phase 2b whole-branch review caught daemon-side; it
/// does not ship again client-side.
#[tokio::test]
async fn every_request_carries_an_explicit_deadline_on_the_wire() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    // fake_client_capturing_envelopes returns a channel of the decoded
    // `Envelope`s the fake daemon received, in arrival order.
    let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;

    let _ = client.request(Request::Ping).await;
    let sent = envelopes.recv().await.unwrap();
    assert_eq!(
        sent.deadline_ms,
        Some(u64::try_from(DEFAULT_DEADLINE.as_millis()).unwrap()),
        "request() must state its deadline, not inherit the daemon's default silently"
    );

    let _ = client
        .request_with_deadline(Request::Ping, Some(START_DEADLINE))
        .await;
    let sent = envelopes.recv().await.unwrap();
    assert_eq!(
        sent.deadline_ms,
        Some(u64::try_from(START_DEADLINE.as_millis()).unwrap())
    );
}
