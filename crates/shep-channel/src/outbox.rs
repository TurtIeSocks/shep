//! The queue between the app's threads and the one thread that writes.
//!
//! Two push policies, because two kinds of message have different costs when
//! they go missing. A dropped metric costs nothing today: the shepherd logs
//! metrics at debug level and no dog reads them. A dropped `ready` hangs a
//! `wait_ready` gate, and a dropped reply costs an operator the whole
//! `action_timeout`. So metrics are lossy and never block the caller, and
//! everything else waits for room.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, PoisonError};

use crate::{ChannelError, ChildMessage};

/// How many messages may wait for the writer before the policy applies.
pub(crate) const DEFAULT_CAPACITY: usize = 1024;

#[derive(Debug)]
struct Inner {
    queue: VecDeque<ChildMessage>,
    dropped: u64,
    closed: bool,
}

/// The bounded queue the writer thread drains.
#[derive(Debug)]
pub(crate) struct Outbox {
    inner: Mutex<Inner>,
    capacity: usize,
    /// Signalled when a message is queued, or the outbox closes.
    queued: Condvar,
    /// Signalled when a message leaves, or the outbox closes.
    drained: Condvar,
}

impl Outbox {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                queue: VecDeque::new(),
                dropped: 0,
                closed: false,
            }),
            capacity,
            queued: Condvar::new(),
            drained: Condvar::new(),
        }
    }

    /// Queues a message that may be dropped. Never blocks, never fails.
    ///
    /// On a full queue the oldest waiting message is discarded and counted.
    /// Oldest rather than newest: a metric's value is a sample, and the
    /// newer sample is the one worth keeping.
    pub(crate) fn push_lossy(&self, message: ChildMessage) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.closed {
            return;
        }
        if inner.queue.len() >= self.capacity {
            inner.queue.pop_front();
            inner.dropped = inner.dropped.saturating_add(1);
        }
        inner.queue.push_back(message);
        self.queued.notify_one();
    }

    /// Queues a message that must not be lost, waiting for room.
    ///
    /// # Errors
    ///
    /// [`ChannelError::Closed`] when the outbox closes while waiting, which
    /// is the shepherd having gone away.
    pub(crate) fn push_blocking(&self, message: ChildMessage) -> Result<(), ChannelError> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        while !inner.closed && inner.queue.len() >= self.capacity {
            inner = self
                .drained
                .wait(inner)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if inner.closed {
            return Err(ChannelError::Closed);
        }
        inner.queue.push_back(message);
        self.queued.notify_one();
        Ok(())
    }

    /// Takes the next message, waiting for one. `None` once closed and empty.
    pub(crate) fn pop(&self) -> Option<ChildMessage> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        while inner.queue.is_empty() && !inner.closed {
            inner = self
                .queued
                .wait(inner)
                .unwrap_or_else(PoisonError::into_inner);
        }
        let taken = inner.queue.pop_front();
        if taken.is_some() {
            self.drained.notify_one();
        }
        taken
    }

    /// Releases every waiter. Idempotent.
    pub(crate) fn close(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.closed = true;
        drop(inner);
        self.queued.notify_all();
        self.drained.notify_all();
    }

    /// How many messages `push_lossy` has discarded.
    pub(crate) fn dropped(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .dropped
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    /// Every wait in this module's tests is bounded by this. A working
    /// outbox answers in microseconds; this is slack for a loaded runner,
    /// not an expected duration.
    const DEADLINE: Duration = Duration::from_secs(5);

    fn metric(value: f64) -> ChildMessage {
        ChildMessage::Metric {
            name: "rps".into(),
            value,
        }
    }

    #[test]
    fn a_full_outbox_drops_the_oldest_metric_and_counts_it() {
        let outbox = Outbox::new(2);
        outbox.push_lossy(metric(1.0));
        outbox.push_lossy(metric(2.0));
        outbox.push_lossy(metric(3.0));

        assert_eq!(outbox.dropped(), 1);
        assert_eq!(outbox.pop(), Some(metric(2.0)));
        assert_eq!(outbox.pop(), Some(metric(3.0)));
    }

    /// fails if `push_blocking` returns while the queue is full. The forcing
    /// mechanism is the channel: the pusher reports only after it returns,
    /// so a `recv_timeout` that times out proves it is still waiting, and
    /// the `pop` that follows is the explicit transition that releases it.
    #[test]
    fn a_must_deliver_push_waits_for_room_and_then_proceeds() {
        let outbox = Arc::new(Outbox::new(1));
        outbox
            .push_blocking(ChildMessage::Ready)
            .expect("first fits");

        let (tx, rx) = mpsc::channel();
        let pusher = Arc::clone(&outbox);
        let handle = std::thread::spawn(move || {
            let outcome = pusher.push_blocking(ChildMessage::Ready);
            tx.send(outcome).expect("report");
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "push_blocking returned while the outbox was full"
        );

        assert_eq!(outbox.pop(), Some(ChildMessage::Ready));
        rx.recv_timeout(DEADLINE)
            .expect("pusher did not proceed")
            .expect("push after room");
        handle.join().expect("pusher panicked");
    }

    /// fails if closing leaves a blocked pusher parked. Without this an app
    /// whose shepherd went away hangs on `ready()` forever.
    #[test]
    fn closing_releases_a_blocked_push_with_an_error() {
        let outbox = Arc::new(Outbox::new(1));
        outbox
            .push_blocking(ChildMessage::Ready)
            .expect("first fits");

        let (tx, rx) = mpsc::channel();
        let pusher = Arc::clone(&outbox);
        let handle = std::thread::spawn(move || {
            tx.send(pusher.push_blocking(ChildMessage::Ready))
                .expect("report");
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "returned too early"
        );
        outbox.close();

        let outcome = rx.recv_timeout(DEADLINE).expect("still parked after close");
        assert!(matches!(outcome, Err(ChannelError::Closed)));
        handle.join().expect("pusher panicked");
    }

    /// fails if `pop` parks forever on a closed empty outbox, which would
    /// leave the writer thread unjoinable at shutdown.
    #[test]
    fn pop_returns_none_once_closed_and_empty() {
        let outbox = Outbox::new(4);
        outbox.close();
        assert_eq!(outbox.pop(), None);
    }

    /// fails if a lossy push on a closed outbox panics or queues. An app
    /// emitting metrics past shutdown is ordinary, not an error.
    #[test]
    fn a_lossy_push_after_close_is_ignored() {
        let outbox = Outbox::new(4);
        outbox.close();
        outbox.push_lossy(metric(1.0));
        assert_eq!(outbox.pop(), None);
        assert_eq!(outbox.dropped(), 0);
    }
}
