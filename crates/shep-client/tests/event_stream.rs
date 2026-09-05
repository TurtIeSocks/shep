//! `Client::subscribe` and the [`EventStream`](shep_client::EventStream) it
//! hands back, driven against the hand-rolled daemon fakes in
//! [`shep_client::testing`].
//!
//! An integration test rather than a `#[cfg(test)] mod tests` block inside
//! `events.rs`, for the reason spelled out at the top of `request_reply.rs`.

use std::time::Duration;

use shep_client::Lagged;
use shep_client::testing::{fake_client_with_push, sample_info};
use shep_core::protocol::{BusEvent, ProcessEventKind, Response};

/// Every `stream.next()` in this file is wrapped in this bound so a broken
/// implementation fails with a named assertion instead of hanging the test
/// run.
const EVENT_TIMEOUT: Duration = Duration::from_secs(1);

#[tokio::test]
async fn subscribe_yields_events_the_daemon_pushes() {
    let dir = tempfile::tempdir().unwrap();
    let path = shep_client::testing::control_address(dir.path());
    let (client, daemon) = fake_client_with_push(&path).await;
    let mut stream = client.subscribe(vec!["log.*".into()]).await.unwrap();

    for i in 0..3u32 {
        daemon
            .push(BusEvent::LogOut {
                id: 1,
                line: format!("line {i}"),
            })
            .await;
    }

    for i in 0..3u32 {
        let event = tokio::time::timeout(EVENT_TIMEOUT, stream.next())
            .await
            .expect("a pushed event must arrive, not hang")
            .unwrap()
            .unwrap();
        assert_eq!(
            event,
            BusEvent::LogOut {
                id: 1,
                line: format!("line {i}")
            },
            "events must arrive in push order"
        );
    }
}

/// `Client::subscribe` installs the local receiver before sending the
/// wire `Request::Subscribe`. Catches an implementation that creates
/// the receiver only after the reply. Against a daemon that writes its
/// event right after replying, the event would vanish. This would
/// hang instead of failing a named assertion. Does not pin
/// reply-before-event ordering on the wire, only that the receiver is
/// installed in time.
#[tokio::test]
async fn subscribing_installs_the_receiver_before_the_request_is_sent() {
    let dir = tempfile::tempdir().unwrap();
    let path = shep_client::testing::control_address(dir.path());
    let (client, daemon) = fake_client_with_push(&path).await;
    daemon.queue_reply_then_event(
        Response::Subscribed,
        BusEvent::Process {
            event: ProcessEventKind::Online,
            info: sample_info(),
            manually: true,
            at_ms: 0,
        },
    );

    let mut stream = tokio::time::timeout(
        Duration::from_secs(1),
        client.subscribe(vec!["process.*".into()]),
    )
    .await
    .expect("subscribe must not deadlock behind its own reply")
    .unwrap();

    assert!(matches!(
        tokio::time::timeout(EVENT_TIMEOUT, stream.next())
            .await
            .expect("the pre-installed receiver must observe the queued event")
            .unwrap()
            .unwrap(),
        BusEvent::Process {
            event: ProcessEventKind::Online,
            ..
        }
    ));
}

/// `RunningDaemon::run` sends `DaemonShutdown` on the bus before
/// closing the sockets. The consumer must see the event before the
/// stream ends. A bad implementation lets closing race the broadcast
/// event, or lets `RecvError::Closed` pre-empt a buffered item.
/// Either way the stream ends cleanly and the caller never sees why.
#[tokio::test]
async fn a_daemon_shutdown_event_ends_the_stream_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let path = shep_client::testing::control_address(dir.path());
    let (client, daemon) = fake_client_with_push(&path).await;
    let mut stream = client.subscribe(vec!["daemon.*".into()]).await.unwrap();

    daemon.push(BusEvent::DaemonShutdown).await;
    daemon.close().await;

    assert_eq!(
        tokio::time::timeout(EVENT_TIMEOUT, stream.next())
            .await
            .expect("the DaemonShutdown event must arrive, not hang")
            .unwrap()
            .unwrap(),
        BusEvent::DaemonShutdown
    );
    assert!(
        tokio::time::timeout(EVENT_TIMEOUT, stream.next())
            .await
            .expect("the stream must end after the notice, not hang")
            .is_none(),
        "the stream ends after the notice, not before it"
    );
}

/// Requires a lag notice somewhere after an overrun, not necessarily
/// first. Nothing synchronises the actor's reads against the
/// consumer's poll, so asserting position would flake. One that maps
/// `RecvError::Lagged` to a silent skip, or to `Poll::Ready(None)`,
/// never produces a lag. It fails this test.
#[tokio::test]
async fn a_lagging_consumer_reports_the_lag_rather_than_silently_skipping() {
    let dir = tempfile::tempdir().unwrap();
    let path = shep_client::testing::control_address(dir.path());
    let (client, daemon) = fake_client_with_push(&path).await;
    let mut stream = client.subscribe(vec!["log.*".into()]).await.unwrap();

    // `overrun_by` pushes the actor's whole broadcast capacity plus this
    // many. The capacity is `pub(crate)`: the published API has no reason
    // to size a buffer against it.
    daemon.overrun_by(8).await;
    daemon.close().await;

    let count = tokio::time::timeout(EVENT_TIMEOUT, async {
        let mut lag = None;
        while let Some(item) = stream.next().await {
            if let Err(Lagged { count }) = item {
                lag = Some(count);
                break;
            }
        }
        lag
    })
    .await
    .expect("must not hang waiting for the lag notice or the stream to end")
    .expect("an overrun must be reported, never silently skipped");
    assert!(count > 0, "the lag notice must say how many were lost");
}
