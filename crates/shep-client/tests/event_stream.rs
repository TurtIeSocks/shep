//! `Client::subscribe` and the [`EventStream`](shep_client::EventStream) it
//! hands back, driven against the hand-rolled daemon fakes in
//! [`shep_client::testing`].
//!
//! An integration test rather than a `#[cfg(test)] mod tests` block inside
//! `events.rs`, for the reason spelled out at the top of `request_reply.rs`.

#![cfg(unix)]

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
    let path = dir.path().join("s.sock");
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

/// `Client::subscribe` installs the local receiver on the actor before sending
/// the wire `Request::Subscribe` (`client.rs:234-244`). Catches an
/// implementation that sends `Request::Subscribe` and only creates the local
/// receiver once the reply comes back: against a daemon that writes its event
/// immediately after replying (`server.rs:357`'s own ordering), that receiver
/// would not exist yet when the event crosses the wire, so the event vanishes
/// and this hangs on the `stream.next()` below instead of a named assertion
/// failing. Does NOT pin reply-before-event ordering on the wire itself — only
/// that the receiver is installed in time to catch whatever the daemon sends
/// once the request goes out.
#[tokio::test]
async fn subscribing_installs_the_receiver_before_the_request_is_sent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
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

/// `RunningDaemon::run` sends `DaemonShutdown` on the bus (boot.rs:719) and
/// only then closes the sockets. The consumer must see the event before the
/// stream ends — an implementation that lets the connection closing race ahead
/// of the already-broadcast event (or that treats the underlying
/// `RecvError::Closed` as capable of pre-empting a buffered item) would report
/// a clean end-of-stream for a daemon that actually went away without the
/// caller ever seeing why.
#[tokio::test]
async fn a_daemon_shutdown_event_ends_the_stream_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
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

/// Overrun the local buffer and require a lag notice somewhere in what comes
/// back. Deliberately NOT "the first item is `Lagged`": nothing synchronises
/// the actor's reads against the consumer's first poll, so the actor may have
/// re-broadcast only a few frames by then and the first item is a normal
/// event. Asserting position would be a flake; asserting presence is the
/// behaviour under test. An implementation that maps `RecvError::Lagged` to a
/// silent skip — or to `Poll::Ready(None)` — never produces one and fails this.
#[tokio::test]
async fn a_lagging_consumer_reports_the_lag_rather_than_silently_skipping() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.sock");
    let (client, daemon) = fake_client_with_push(&path).await;
    let mut stream = client.subscribe(vec!["log.*".into()]).await.unwrap();

    // `overrun_by` pushes the actor's whole broadcast capacity plus this
    // many. The capacity itself is `pub(crate)`, deliberately: a consumer of
    // the published API has no business sizing a buffer against it, and the
    // one caller that does need the figure is the fake, from inside the crate.
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
