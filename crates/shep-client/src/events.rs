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

use core::fmt;
use core::pin::Pin;
use core::task::{Context, Poll};

use futures_util::{Stream, StreamExt};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use shep_core::protocol::BusEvent;

/// Live bus events for one [`crate::Client::subscribe`] call.
///
/// Named rather than `impl Stream`: an anonymous return type from a
/// method can't be spelled in a struct field or a function's own return
/// type, both of which a caller holding a subscription needs.
///
/// Each item is `Ok(`[`BusEvent`]`)`, or `Err(`[`Lagged`]`)` when this
/// stream's own local buffer fell behind (see [`Lagged`]'s doc for how
/// that differs from [`BusEvent::Dropped`]). The stream ends once the
/// connection closes; a `Lagged` item does not end it.
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

    /// Returns the next event, or `None` once the subscription ends.
    ///
    /// Inherent so a caller needs no `StreamExt` import: an inherent
    /// method wins name resolution over a trait method of the same name.
    pub async fn next(&mut self) -> Option<Result<BusEvent, Lagged>> {
        StreamExt::next(self).await
    }
}

impl Stream for EventStream {
    type Item = Result<BusEvent, Lagged>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // `BroadcastStreamRecvError` has exactly one variant and is not
        // `#[non_exhaustive]`; no wildcard arm, so a new variant fails this build.
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
/// Distinct from [`BusEvent::Dropped`], which is the daemon's own
/// per-subscriber queue overflowing before an event reaches this client:
/// that arrives as an ordinary `Ok(BusEvent::Dropped { .. })` item on this
/// same stream. `Lagged` is the local channel discarding entries because
/// the [`EventStream`] wasn't polled quickly enough to keep up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lagged {
    /// How many events were discarded since this stream's last successful
    /// read.
    pub count: u64,
}

impl fmt::Display for Lagged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} events dropped locally (lagged)", self.count)
    }
}

impl core::error::Error for Lagged {}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Lagged` implements `Display` and `core::error::Error` like this
    /// crate's other error types, so a consumer can print it or `?` it
    /// into `anyhow`.
    #[test]
    fn lagged_is_printable_and_a_real_error() {
        let lagged = Lagged { count: 7 };
        assert_eq!(lagged.to_string(), "7 events dropped locally (lagged)");

        // compiles only if `Lagged: core::error::Error`; `to_string()` alone needs only `Display`
        let _: &dyn core::error::Error = &lagged;
    }
}
