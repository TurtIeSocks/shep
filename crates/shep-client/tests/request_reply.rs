//! `Client` request/reply routing, error surfacing and deadlines,
//! driven against the hand-rolled daemon fakes in
//! [`shep_client::testing`].
//!
//! An integration test rather than a unit test in `client.rs`. Every
//! assertion reaches only the published surface, linking `shep-client`
//! the way a real embedder does.
//!
//! Needs `--features test-support`. A bare `cargo test -p shep-client`
//! skips this target rather than failing to build it.

use std::time::Duration;

use shep_client::testing::{
    fake_client_capturing_envelopes, fake_client_event_then_reply, fake_client_on,
    fake_client_out_of_order, fake_client_replying_err, fake_client_that_closes_after_handshake,
    fake_client_that_dies_mid_request, fake_client_that_never_replies,
};
use shep_client::{DEADLINE_GRACE, DEFAULT_DEADLINE, RequestError, START_DEADLINE};
use shep_core::protocol::{Request, Response, RpcErrorCode};

#[tokio::test]
async fn two_concurrent_requests_each_get_their_own_reply() {
    let dir = tempfile::tempdir().unwrap();
    let path = shep_client::testing::control_address(dir.path());
    // Replies out of order (id 2 first) to prove routing is by id, not
    // arrival order.
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
    let path = shep_client::testing::control_address(dir.path());
    // Sends BusEvent::Process before Reply{id:1}: the real supervisor's
    // ordering, not a contrived one.
    let (client, _served) = fake_client_event_then_reply(&path).await;
    assert!(matches!(
        client.request(Request::Ping).await.unwrap(),
        Response::Pong
    ));
}

#[tokio::test]
async fn a_daemon_side_error_reply_becomes_a_typed_rpc_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = shep_client::testing::control_address(dir.path());
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
    let path = shep_client::testing::control_address(dir.path());
    let (client, served) = fake_client_that_closes_after_handshake(&path).await;
    served.await.unwrap();
    assert!(matches!(
        client.request(Request::Ping).await,
        Err(RequestError::Closed)
    ));
}

/// Unlike the test above, the request is already in the actor's
/// `pending` map. The connection dies only after the fake daemon reads
/// it, proving the write succeeded. Exercises `actor.rs`'s
/// drain-on-close loop.
#[tokio::test]
async fn a_connection_dying_mid_request_fails_the_pending_request_instead_of_hanging() {
    let dir = tempfile::tempdir().unwrap();
    let path = shep_client::testing::control_address(dir.path());
    let (client, _served) = fake_client_that_dies_mid_request(&path).await;
    assert!(matches!(
        client.request(Request::Ping).await,
        Err(RequestError::Closed)
    ));
}

#[tokio::test(start_paused = true)]
async fn a_deadline_expires_client_side_when_the_daemon_never_answers() {
    let dir = tempfile::tempdir().unwrap();
    let path = shep_client::testing::control_address(dir.path());
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

/// A `kill` polling the wrong path would wait out its teardown budget
/// and report "still tearing down" against a daemon that already shut
/// down cleanly.
#[tokio::test]
async fn a_client_remembers_the_path_it_connected_through() {
    let dir = tempfile::tempdir().unwrap();
    let path = shep_client::testing::control_address(dir.path());
    let (client, _served) = fake_client_on(&path).await;
    assert_eq!(client.socket(), path);
}

/// Nothing else here reads the envelope the client sent. An
/// implementation that always sends `deadline_ms: None` would pass
/// every test here. It would silently inherit the daemon's default.
#[tokio::test]
async fn every_request_carries_an_explicit_deadline_on_the_wire() {
    let dir = tempfile::tempdir().unwrap();
    let path = shep_client::testing::control_address(dir.path());
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

/// Fails if a caller must drain the envelope channel to keep getting
/// replies.
///
/// `fake_client_answering` exists for a caller that sends several
/// requests and reads the record afterwards. Ten requests here against
/// `SCRIPT_CHANNEL_CAPACITY` of eight. An undrained bounded channel
/// would hang this test rather than fail it.
#[tokio::test]
async fn a_run_of_requests_is_answered_without_the_receiver_being_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = shep_client::testing::control_address(dir.path());
    let (client, mut envelopes) =
        shep_client::testing::fake_client_answering(&path, |_| Response::Pong).await;

    for _ in 0..10 {
        client
            .request(Request::Ping)
            .await
            .expect("every request is answered, however far behind the observer is");
    }

    let mut seen = 0;
    while envelopes.try_recv().is_ok() {
        seen += 1;
    }
    assert_eq!(seen, 10, "and every envelope was still recorded");
}

/// `Duration::MAX` is a caller's to pass, and `Duration`'s `Add` panics
/// on overflow rather than saturating.
#[tokio::test]
async fn a_deadline_at_the_duration_ceiling_errors_instead_of_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let path = shep_client::testing::control_address(dir.path());
    let (client, _served) = fake_client_that_dies_mid_request(&path).await;
    assert!(matches!(
        client
            .request_with_deadline(Request::Ping, Some(Duration::MAX))
            .await,
        Err(RequestError::Closed)
    ));
}
