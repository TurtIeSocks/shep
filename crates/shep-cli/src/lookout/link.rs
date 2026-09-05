//! The link task: subscribe for latency, poll for correctness, repair on a
//! drop, climb a bounded ladder on a disconnect, and freeze when it runs out.
//!
//! `tokio::sync::broadcast` drops what a lagging subscriber cannot keep up
//! with, surfaced as `BusEvent::Dropped` or `shep_client::Lagged`. Both
//! trigger an immediate poll; a drop carries no detail, so re-listing is the
//! only repair.
//!
//! A dead shepherd freezes the dashboard rather than exiting it: after
//! [`RECONNECT_ATTEMPTS`] rungs this sends [`Msg::Frozen`] and ends, and the
//! last known flock stays on screen.

use core::fmt;
use std::time::Duration;

use shep_client::{Lagged, RequestError};
use shep_core::protocol::BusEvent;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use super::app::Msg;
use super::source::{EventSource, FlockSource, Shepherd};

/// How often the flock is re-listed when nothing has gone wrong.
///
/// Two seconds: a `process.*` event already updates the pane within
/// milliseconds, so this only repairs drift. A `ListFlock` is a map lookup and
/// one frame each way.
pub const FLOCK_POLL: Duration = Duration::from_secs(2);

/// How many delayed re-dials the link announces as `Retrying` before it
/// gives up and freezes on the next attempt.
///
/// Five, at [`RECONNECT_FIRST_WAIT`] doubling to [`RECONNECT_MAX_WAIT`], is
/// 7.75 seconds of waiting: long enough to cover a `shep kill` then `shep
/// muster`, or a systemd restart, short enough that a shepherd which is
/// genuinely gone is declared gone while the operator is still watching.
pub const RECONNECT_ATTEMPTS: u32 = 5;

/// The wait before the first re-dial.
pub const RECONNECT_FIRST_WAIT: Duration = Duration::from_millis(250);

/// The ceiling on the doubling.
pub const RECONNECT_MAX_WAIT: Duration = Duration::from_secs(4);

/// The dashboard stopped listening: its [`Msg`] channel is closed.
///
/// The only condition [`run_connected`] reports that is not a reconnect. Named
/// rather than `()`, since an exported `Result<_, ()>` trips
/// `clippy::result_unit_err`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiGone;

impl fmt::Display for UiGone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the dashboard stopped listening")
    }
}

impl core::error::Error for UiGone {}

/// The receivers one connection borrows from the ladder and hands back when it
/// ends.
#[derive(Debug)]
pub struct Channels {
    /// Out-of-band poll requests: the `r` key, and the reducer's own
    /// [`super::app::Effect::PollNow`].
    pub polls: mpsc::Receiver<()>,
    /// One-shot requests the dashboard wants sent on this connection.
    pub requests: mpsc::Receiver<super::app::Sent>,
}

/// Runs `opened` and everything that replaces it, until the ladder runs out.
///
/// Sends [`Msg`]s to the UI over `msgs`, and takes a request for an
/// out-of-band poll and a one-shot request to send, both on `channels`.
///
/// `opened` is handed in already connected: the first dial belongs to
/// `super::lookout`, which makes it before entering raw mode so a shepherd
/// that was never running refuses with `daemon_unreachable`, exit 5, and no
/// alternate screen.
///
/// Ends only after sending [`Msg::Frozen`], or when the UI stops listening.
pub async fn run_link<S: Shepherd>(
    mut shepherd: S,
    opened: (S::Flock, S::Events),
    msgs: mpsc::Sender<Msg>,
    mut channels: Channels,
    period: Duration,
) {
    let mut attempt = 0u32;
    let mut wait = RECONNECT_FIRST_WAIT;
    let mut connection = Some(opened);

    loop {
        let (flock, events) = match connection.take() {
            Some(pair) => pair,
            None => match shepherd.link().await {
                Ok(pair) => pair,
                Err(err) => {
                    // Not surfaced as its own Msg: the reducer's `Retrying`
                    // state is what the banner reads, and a per-attempt error
                    // string would change the sentence every 250ms.
                    let _ = err;
                    attempt += 1;
                    if attempt > RECONNECT_ATTEMPTS {
                        let _ = msgs
                            .send(Msg::Frozen {
                                at_local: local_now(),
                            })
                            .await;
                        return;
                    }
                    let _ = msgs.send(Msg::Retrying { attempt }).await;
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(RECONNECT_MAX_WAIT);
                    continue;
                }
            },
        };

        // Gated on whether a `Retrying` was announced, not on whether a
        // connection was ever opened: `Relinked` is what takes that banner
        // down again.
        if attempt > 0 && msgs.send(Msg::Relinked).await.is_err() {
            return;
        }
        attempt = 0;
        wait = RECONNECT_FIRST_WAIT;

        match run_connected(flock, events, msgs.clone(), channels, period).await {
            // `channels` comes back by value, not by `&mut`: `run_connected`
            // is `tokio::spawn`ed directly by its own tests, and a spawned
            // future cannot borrow a caller's local.
            Ok(returned) => channels = returned,
            // The UI is gone. Nothing left to report to.
            Err(UiGone) => return,
        }
    }
}

/// One connection's lifetime: an opening listing, then subscribe-and-poll
/// until the subscription ends.
///
/// Returns `channels` on `Ok`, for the next rung of the ladder.
/// `Shepherd::link` has already subscribed by the time this is called, so an
/// event arriving before the first snapshot upserts into an empty map.
///
/// # Errors
/// [`UiGone`] when the dashboard's own [`Msg`] channel has closed. A failed
/// poll, a dropped frame, a lagging receiver and the subscription ending are
/// all handled in place or handed back to the ladder as `Ok`.
pub async fn run_connected<F: FlockSource, E: EventSource>(
    flock: F,
    mut events: E,
    msgs: mpsc::Sender<Msg>,
    mut channels: Channels,
    period: Duration,
) -> Result<Channels, UiGone> {
    // Every connection begins with one listing, cold start or reconnect alike.
    reconcile(&flock, &msgs).await?;

    // `interval_at`, not `interval`: a plain interval yields its first tick at
    // once, which would make the first scheduled poll unattributable.
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    // A dashboard that fell behind must not then fire a burst of catch-up polls
    // at a shepherd that is probably why it fell behind.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ticker.tick() => reconcile(&flock, &msgs).await?,
            _ = channels.polls.recv() => reconcile(&flock, &msgs).await?,
            // `Some(sent) = ...`, not `_ = ...`: a receiver whose senders have
            // all been dropped is `Ready(None)` forever, and a `_` pattern
            // would spin this loop. A pattern that fails disables the branch.
            Some(sent) = channels.requests.recv() => {
                // Awaited inline, holding the other arms for the request's
                // duration, bounded by the client's own deadline.
                let result = flock.send(sent.request()).await;
                msgs.send(Msg::Replied { sent, result })
                    .await
                    .map_err(|_| UiGone)?;
            }
            next = events.next_event() => match next {
                // The subscription ended: the connection is gone. Back to the
                // ladder.
                None => return Ok(channels),
                Some(Ok(event)) => {
                    // `Dropped` is a named variant this binary understands,
                    // so it gets a repair as well as being forwarded.
                    let repair = matches!(event, BusEvent::Dropped { .. });
                    msgs.send(Msg::Event(event)).await.map_err(|_| UiGone)?;
                    if repair {
                        reconcile(&flock, &msgs).await?;
                    }
                }
                Some(Err(Lagged { count })) => {
                    msgs.send(Msg::BusLagged { count })
                        .await
                        .map_err(|_| UiGone)?;
                    reconcile(&flock, &msgs).await?;
                }
            },
        }
    }
}

/// One listing, forwarded as a snapshot.
///
/// A failed poll is dropped rather than propagated: one bad round trip must
/// not take the connection down, and a dead connection ends its subscription,
/// which is the condition that does climb the ladder.
async fn reconcile<F: FlockSource>(flock: &F, msgs: &mpsc::Sender<Msg>) -> Result<(), UiGone> {
    match flock.flock().await {
        Ok(rows) => msgs
            .send(Msg::Snapshot {
                rows,
                at: std::time::Instant::now(),
            })
            .await
            .map_err(|_| UiGone),
        Err(RequestError::Closed) => Ok(()),
        Err(_other) => Ok(()),
    }
}

/// The wall-clock moment of a freeze, already formatted.
///
/// Local, not UTC: read during an incident, at a terminal.
fn local_now() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use shep_core::protocol::{ProcessEventKind, ProcessInfo, Request, Response, SelectorSpec};
    use shep_core::status::ProcStatus;
    use tokio::sync::broadcast;

    use crate::lookout::app::Sent;
    use crate::lookout::source::LinkError;

    fn sheep(id: u32) -> ProcessInfo {
        ProcessInfo::builder(id, format!("sheep-{id}"), ProcStatus::Online).build()
    }

    /// A flock source that counts polls, so a test can assert why one happened.
    struct CountingFlock {
        polls: Arc<AtomicU64>,
    }

    impl FlockSource for CountingFlock {
        async fn flock(&self) -> Result<Vec<ProcessInfo>, RequestError> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![sheep(1)])
        }

        async fn send(&self, _request: Request) -> Result<Response, RequestError> {
            Ok(Response::Described(Vec::new()))
        }
    }

    /// A real `broadcast::Receiver` with a tiny capacity, so the bus genuinely
    /// drops frames.
    struct BroadcastEvents(broadcast::Receiver<BusEvent>);

    impl EventSource for BroadcastEvents {
        async fn next_event(&mut self) -> Option<Result<BusEvent, Lagged>> {
            match self.0.recv().await {
                Ok(event) => Some(Ok(event)),
                Err(broadcast::error::RecvError::Lagged(count)) => Some(Err(Lagged { count })),
                Err(broadcast::error::RecvError::Closed) => None,
            }
        }
    }

    /// The property the two-route design exists for: `broadcast` drops for a
    /// subscriber that falls behind, so a dashboard that only subscribed would
    /// go silently wrong under load. The wait is bounded, so a link task that
    /// never polls fails this test rather than hanging it.
    #[tokio::test(start_paused = true)]
    async fn a_lagging_subscriber_polls_immediately_instead_of_waiting() {
        let (tx, rx) = broadcast::channel(2);
        let polls = Arc::new(AtomicU64::new(0));
        let (msg_tx, mut msg_rx) = mpsc::channel(64);
        let (_poll_tx, poll_rx) = mpsc::channel(1);
        let (_request_tx, request_rx) = mpsc::channel(2);

        // Overrun the capacity-2 channel before the loop ever reads it.
        for id in 0..8 {
            let _ = tx.send(BusEvent::Process {
                event: ProcessEventKind::Online,
                info: sheep(id),
                manually: false,
                at_ms: 0,
            });
        }

        let flock = CountingFlock {
            polls: Arc::clone(&polls),
        };
        let task = tokio::spawn(run_connected(
            flock,
            BroadcastEvents(rx),
            msg_tx,
            Channels {
                polls: poll_rx,
                requests: request_rx,
            },
            // Far longer than this test, so any poll past the opening
            // listing is attributable to the lag.
            Duration::from_secs(3600),
        ));

        // Waits for the snapshot the repair produces, not the `BusLagged`
        // notice: the notice is forwarded first, so stopping there would read
        // the counter in a race with the poll it counts.
        let mut saw_lagged = false;
        let mut repaired = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !(saw_lagged && repaired) {
            let Ok(Some(msg)) = tokio::time::timeout_at(deadline, msg_rx.recv()).await else {
                break;
            };
            match msg {
                Msg::BusLagged { .. } => saw_lagged = true,
                Msg::Snapshot { .. } if saw_lagged => repaired = true,
                _ => {}
            }
        }
        task.abort();

        assert!(saw_lagged, "the lag reached the reducer");
        assert!(
            repaired,
            "the lag was repaired by a listing rather than left to the interval"
        );
        assert_eq!(
            polls.load(Ordering::SeqCst),
            2,
            "the opening listing, plus exactly one repair for the lag; the one-hour interval caused none"
        );
    }

    /// `Dropped` is a named `BusEvent` this binary understands, so it must not
    /// fall into the catch-all arm for variants a newer shepherd added.
    #[tokio::test(start_paused = true)]
    async fn a_shepherd_side_drop_polls_and_is_forwarded() {
        let (tx, rx) = broadcast::channel(16);
        let polls = Arc::new(AtomicU64::new(0));
        let (msg_tx, mut msg_rx) = mpsc::channel(64);
        let (_poll_tx, poll_rx) = mpsc::channel(1);
        let (_request_tx, request_rx) = mpsc::channel(2);
        let _ = tx.send(BusEvent::Dropped { count: 9 });

        let task = tokio::spawn(run_connected(
            CountingFlock {
                polls: Arc::clone(&polls),
            },
            BroadcastEvents(rx),
            msg_tx,
            Channels {
                polls: poll_rx,
                requests: request_rx,
            },
            Duration::from_secs(3600),
        ));

        // The first message is the opening listing's snapshot, not the drop,
        // so this scans rather than asserting on message one.
        let mut forwarded = false;
        let mut repaired = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !(forwarded && repaired) {
            let Ok(Some(msg)) = tokio::time::timeout_at(deadline, msg_rx.recv()).await else {
                break;
            };
            match msg {
                Msg::Event(BusEvent::Dropped { count: 9 }) => forwarded = true,
                Msg::Snapshot { .. } if forwarded => repaired = true,
                _ => {}
            }
        }
        task.abort();

        assert!(forwarded, "the drop reached the reducer");
        assert!(repaired, "and it triggered a repair listing");
        assert_eq!(
            polls.load(Ordering::SeqCst),
            2,
            "the opening listing, plus one repair for the drop"
        );
    }

    /// Bounded retry, then a message saying the shepherd has died, then
    /// nothing. Never an exit.
    ///
    /// `start_paused` so the ladder's waits cost no wall clock; the `timeout`
    /// bound is real, since never giving up is the regression this catches.
    #[tokio::test(start_paused = true)]
    async fn the_ladder_is_bounded_and_ends_frozen() {
        struct NeverConnects {
            attempts: Arc<AtomicU64>,
        }

        impl Shepherd for NeverConnects {
            type Flock = CountingFlock;
            type Events = BroadcastEvents;

            async fn link(&mut self) -> Result<(Self::Flock, Self::Events), LinkError> {
                self.attempts.fetch_add(1, Ordering::SeqCst);
                Err(LinkError::Unreachable("nothing is listening".to_string()))
            }
        }

        let attempts = Arc::new(AtomicU64::new(0));
        let (msg_tx, mut msg_rx) = mpsc::channel(64);
        let (_poll_tx, poll_rx) = mpsc::channel(1);
        let (_request_tx, request_rx) = mpsc::channel(2);

        // The opening connection is handed in and ends at once: its sender is
        // dropped, so the subscription closes on the first read and the ladder
        // takes over.
        let (opening_tx, opening_rx) = broadcast::channel(1);
        drop(opening_tx);

        let task = tokio::spawn(run_link(
            NeverConnects {
                attempts: Arc::clone(&attempts),
            },
            (
                CountingFlock {
                    polls: Arc::new(AtomicU64::new(0)),
                },
                BroadcastEvents(opening_rx),
            ),
            msg_tx,
            Channels {
                polls: poll_rx,
                requests: request_rx,
            },
            Duration::from_secs(2),
        ));

        let mut seen = Vec::new();
        let done = tokio::time::timeout(Duration::from_secs(120), async {
            while let Some(msg) = msg_rx.recv().await {
                let frozen = matches!(msg, Msg::Frozen { .. });
                seen.push(msg);
                if frozen {
                    break;
                }
            }
        })
        .await;
        assert!(
            done.is_ok(),
            "the ladder gave up rather than retrying forever"
        );

        // One immediate re-dial the moment the connection ended, then
        // RECONNECT_ATTEMPTS more behind the waits. Only the delayed ones
        // announce a `Retrying`, so the two counts differ by exactly one.
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            u64::from(RECONNECT_ATTEMPTS) + 1
        );
        let retries = seen
            .iter()
            .filter(|msg| matches!(msg, Msg::Retrying { .. }))
            .count();
        assert_eq!(retries, usize::try_from(RECONNECT_ATTEMPTS).unwrap());
        assert!(matches!(seen.last(), Some(Msg::Frozen { .. })));

        // And it ends: a link task alive after a freeze would keep a dead
        // connection's machinery running behind a screen that says it is gone.
        let ended = tokio::time::timeout(Duration::from_secs(5), task).await;
        assert!(ended.is_ok(), "the link task ended after freezing");
    }

    /// A relink that reconnected but kept the dead subscription would leave
    /// the dashboard live-looking and permanently stale.
    #[tokio::test(start_paused = true)]
    async fn a_successful_relink_reports_live_on_the_first_success() {
        struct FailsOnce {
            done: bool,
            polls: Arc<AtomicU64>,
            /// Holds the reconnected subscription's sender open. Dropping it
            /// ends that stream on its first read, so the ladder goes round
            /// again and the `Relinked` comes from the wrong cycle.
            keepalive: Option<broadcast::Sender<BusEvent>>,
        }

        impl Shepherd for FailsOnce {
            type Flock = CountingFlock;
            type Events = BroadcastEvents;

            async fn link(&mut self) -> Result<(Self::Flock, Self::Events), LinkError> {
                if self.done {
                    let (tx, rx) = broadcast::channel(16);
                    self.keepalive = Some(tx);
                    return Ok((
                        CountingFlock {
                            polls: Arc::clone(&self.polls),
                        },
                        BroadcastEvents(rx),
                    ));
                }
                self.done = true;
                Err(LinkError::Unreachable("not yet".to_string()))
            }
        }

        let polls = Arc::new(AtomicU64::new(0));
        let (msg_tx, mut msg_rx) = mpsc::channel(64);
        let (_poll_tx, poll_rx) = mpsc::channel(1);
        let (_request_tx, request_rx) = mpsc::channel(2);

        // The opening connection ends immediately, with its own counter, so
        // `polls` below counts only what the reconnected one listed.
        let (opening_tx, opening_rx) = broadcast::channel(1);
        drop(opening_tx);

        let task = tokio::spawn(run_link(
            FailsOnce {
                done: false,
                polls: Arc::clone(&polls),
                keepalive: None,
            },
            (
                CountingFlock {
                    polls: Arc::new(AtomicU64::new(0)),
                },
                BroadcastEvents(opening_rx),
            ),
            msg_tx,
            Channels {
                polls: poll_rx,
                requests: request_rx,
            },
            Duration::from_secs(2),
        ));

        let mut retried = false;
        let mut relinked = false;
        let mut listed_after_relink = false;
        let _ = tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(msg) = msg_rx.recv().await {
                match msg {
                    Msg::Retrying { attempt: 1 } => retried = true,
                    Msg::Relinked => relinked = true,
                    Msg::Snapshot { .. } if relinked => {
                        listed_after_relink = true;
                        break;
                    }
                    _ => {}
                }
            }
        })
        .await;
        task.abort();

        assert!(retried, "the failed dial put the banner up");
        assert!(
            relinked,
            "and the FIRST successful re-dial took it down again"
        );
        assert!(
            listed_after_relink,
            "the fresh connection re-listed the flock"
        );
        assert_eq!(
            polls.load(Ordering::SeqCst),
            1,
            "exactly the reconnected connection's opening listing"
        );
    }

    /// The first poll must be attributable: to the opening listing, to a drop,
    /// or to the interval elapsing, never to `interval`'s first tick at zero.
    #[tokio::test(start_paused = true)]
    async fn the_scheduled_poll_lands_on_the_interval_and_not_at_zero() {
        let (tx, rx) = broadcast::channel(16);
        let polls = Arc::new(AtomicU64::new(0));
        let (msg_tx, _msg_rx) = mpsc::channel(256);
        let (_poll_tx, poll_rx) = mpsc::channel(1);
        let (_request_tx, request_rx) = mpsc::channel(2);
        let task = tokio::spawn(run_connected(
            CountingFlock {
                polls: Arc::clone(&polls),
            },
            BroadcastEvents(rx),
            msg_tx,
            Channels {
                polls: poll_rx,
                requests: request_rx,
            },
            Duration::from_secs(2),
        ));

        tokio::time::sleep(Duration::from_millis(1900)).await;
        assert_eq!(
            polls.load(Ordering::SeqCst),
            1,
            "the opening listing, and NOTHING from the timer before its period elapsed"
        );
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert_eq!(
            polls.load(Ordering::SeqCst),
            3,
            "the opening listing, plus t=2s and t=4s"
        );
        drop(tx);
        task.abort();
    }

    /// A flock source that records every `send` request it was asked, and
    /// answers each with one row.
    struct RecordingFlock {
        seen: Arc<std::sync::Mutex<Vec<Request>>>,
    }

    impl FlockSource for RecordingFlock {
        async fn flock(&self) -> Result<Vec<ProcessInfo>, RequestError> {
            Ok(vec![sheep(1)])
        }

        async fn send(&self, request: Request) -> Result<Response, RequestError> {
            self.seen.lock().unwrap().push(request.clone());
            Ok(Response::Described(vec![sheep(1)]))
        }
    }

    /// The echo tag is what routes a reply with no correlation id, including
    /// an `Err` that carries no shape of its own. The wait is bounded.
    #[tokio::test(start_paused = true)]
    async fn a_request_reaches_the_shepherd_and_its_reply_comes_back() {
        let (msg_tx, mut msg_rx) = mpsc::channel(64);
        let (_poll_tx, poll_rx) = mpsc::channel(1);
        let (request_tx, request_rx) = mpsc::channel(2);
        let (_events_tx, events_rx) = broadcast::channel(4);

        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let flock = RecordingFlock {
            seen: Arc::clone(&seen),
        };
        let task = tokio::spawn(run_connected(
            flock,
            BroadcastEvents(events_rx),
            msg_tx,
            Channels {
                polls: poll_rx,
                requests: request_rx,
            },
            Duration::from_secs(3600),
        ));
        request_tx.send(Sent::Lambs { id: 7 }).await.unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut answered = None;
        while answered.is_none() {
            let Ok(Some(msg)) = tokio::time::timeout_at(deadline, msg_rx.recv()).await else {
                break;
            };
            if let Msg::Replied { sent, result } = msg {
                answered = Some((sent, result));
            }
        }
        task.abort();

        let (sent, result) = answered.expect("the reply came back");
        assert_eq!(sent, Sent::Lambs { id: 7 }, "tagged with what it answered");
        assert!(matches!(result, Ok(Response::Described(_))));
        assert_eq!(
            *seen.lock().unwrap(),
            vec![Request::Describe {
                selector: SelectorSpec::Id(7)
            }],
            "the id it was asked about, as a selector, and nothing else"
        );
    }
}
