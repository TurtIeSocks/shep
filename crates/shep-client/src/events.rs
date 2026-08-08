//! [`EventStream`], the public subscription surface handed back by
//! [`crate::Client::subscribe`], and [`Lagged`], the local-only drop notice
//! it can yield.
//!
//! Wraps [`tokio_stream::wrappers::BroadcastStream`] rather than polling a
//! raw [`broadcast::Receiver`] by hand: that receiver exposes no poll API at
//! all (`recv` is `async`, and `try_recv` registers no waker, so a
//! hand-rolled `poll_next` over it either does not compile or busy-spins).
//! `BroadcastStream` is upstream's answer, built on tokio-util's
//! `ReusableBoxFuture`, which owns the receiver between polls.

use core::pin::Pin;
use core::task::{Context, Poll};

use futures_util::Stream;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use shep_core::protocol::BusEvent;

/// Live bus events for one [`crate::Client::subscribe`] call.
///
/// Named rather than `impl Stream` (IR-15): an anonymous `impl Trait`
/// return type from a *method* cannot be spelled in a struct field or a
/// function's own return type, both of which a caller holding onto a
/// subscription routinely needs.
///
/// Each item is `Ok(`[`BusEvent`]`)` for a normal event, or
/// `Err(`[`Lagged`]`)` when this stream's own local buffer fell behind —
/// see [`Lagged`]'s doc for how that differs from
/// [`BusEvent::Dropped`]. The stream ends (yields `None`) once the
/// connection it was subscribed over closes; nothing about a `Lagged` item
/// ends it — a lagging consumer keeps yielding events after catching up.
#[derive(Debug)]
pub struct EventStream {
    inner: BroadcastStream<BusEvent>,
}

impl EventStream {
    /// Wraps `receiver` in the named public stream type.
    pub(crate) fn new(receiver: broadcast::Receiver<BusEvent>) -> Self {
        Self {
            inner: BroadcastStream::new(receiver),
        }
    }
}

impl Stream for EventStream {
    type Item = Result<BusEvent, Lagged>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // `BroadcastStreamRecvError` has exactly one variant today (verified
        // against tokio-stream 0.1.19) and is not `#[non_exhaustive]`, so this
        // match has no wildcard arm on purpose: a variant added upstream
        // should fail this build loudly rather than silently fall through.
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(event))) => Poll::Ready(Some(Ok(event))),
            Poll::Ready(Some(Err(BroadcastStreamRecvError::Lagged(count)))) => {
                Poll::Ready(Some(Err(Lagged { count })))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// The local [`broadcast::Receiver`] behind an [`EventStream`] fell behind
/// and discarded events before this stream could read them.
///
/// Distinct from [`BusEvent::Dropped`], which is the *daemon's* own
/// per-subscriber queue overflowing before an event ever reached this
/// client — that condition arrives as an ordinary `Ok(BusEvent::Dropped {
/// .. })` item on this same stream, no different from any other
/// [`BusEvent`]. `Lagged` is the opposite failure: the event made it across
/// the wire and into this connection's local broadcast channel just fine,
/// but the [`EventStream`] reading from it was not polled quickly enough to
/// keep up (another slow subscriber on the same channel, or the channel's
/// own bounded capacity), and the channel discarded its oldest entries to
/// make room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lagged {
    /// How many events were discarded since this stream's last successful
    /// read.
    pub count: u64,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::StreamExt;
    use shep_core::protocol::{BusEvent, ProcessEventKind, Response};

    use super::Lagged;
    use crate::actor::EVENT_CHANNEL_CAPACITY;
    use crate::testing::{fake_client_with_push, sample_info};

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
            let event = stream.next().await.unwrap().unwrap();
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

    /// `server.rs:357` sends the `Subscribed` reply ahead of any event, by
    /// queue order. The client must have routed that reply before the first
    /// event reaches the stream — an implementation that waits for the
    /// reply *after* installing the subscriber deadlocks against a daemon
    /// that pushes fast. Catches an implementation that sends
    /// `Request::Subscribe` and only creates the local receiver once the
    /// reply comes back: that ordering can never observe the event at all
    /// (the daemon already wrote it before the receiver existed) and would
    /// hang on the `stream.next()` below instead of the timeout tripping.
    #[tokio::test]
    async fn the_subscribed_reply_arrives_before_any_event() {
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
            stream.next().await.unwrap().unwrap(),
            BusEvent::Process {
                event: ProcessEventKind::Online,
                ..
            }
        ));
    }

    /// `RunningDaemon::run` sends `DaemonShutdown` on the bus (boot.rs:719)
    /// and only then closes the sockets. The consumer must see the event
    /// before the stream ends — an implementation that lets the connection
    /// closing race ahead of the already-broadcast event (or that treats
    /// the underlying `RecvError::Closed` as capable of pre-empting a
    /// buffered item) would report a clean end-of-stream for a daemon that
    /// actually went away without the caller ever seeing why.
    #[tokio::test]
    async fn a_daemon_shutdown_event_ends_the_stream_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, daemon) = fake_client_with_push(&path).await;
        let mut stream = client.subscribe(vec!["daemon.*".into()]).await.unwrap();

        daemon.push(BusEvent::DaemonShutdown).await;
        daemon.close().await;

        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            BusEvent::DaemonShutdown
        );
        assert!(
            stream.next().await.is_none(),
            "the stream ends after the notice, not before it"
        );
    }

    /// Overrun the local buffer and require a lag notice somewhere in what
    /// comes back. Deliberately NOT "the first item is `Lagged`": nothing
    /// synchronises the actor's reads against the consumer's first poll, so
    /// the actor may have re-broadcast only a few frames by then and the
    /// first item is a normal event. Asserting position would be a flake;
    /// asserting presence is the behaviour under test. An implementation
    /// that maps `RecvError::Lagged` to a silent skip — or to
    /// `Poll::Ready(None)` — never produces one and fails this.
    #[tokio::test]
    async fn a_lagging_consumer_reports_the_lag_rather_than_silently_skipping() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, daemon) = fake_client_with_push(&path).await;
        let mut stream = client.subscribe(vec!["log.*".into()]).await.unwrap();

        let overrun = EVENT_CHANNEL_CAPACITY + 8;
        for i in 0..overrun {
            daemon
                .push(BusEvent::LogOut {
                    id: 1,
                    line: i.to_string(),
                })
                .await;
        }
        daemon.close().await;

        let mut lag = None;
        while let Some(item) = stream.next().await {
            if let Err(Lagged { count }) = item {
                lag = Some(count);
                break;
            }
        }
        let count = lag.expect("an overrun must be reported, never silently skipped");
        assert!(count > 0, "the lag notice must say how many were lost");
    }
}
