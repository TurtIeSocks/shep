//! The link task: subscribe for latency, poll for correctness, repair on a
//! drop, climb a bounded ladder on a disconnect, and freeze when it runs out.
//!
//! **The bus drops events, and that is what this module exists to survive.**
//! `tokio::sync::broadcast` discards what a lagging subscriber cannot keep up
//! with rather than queueing it — the shepherd surfaces that as
//! `BusEvent::Dropped`, and this process's own receiver can lag the same way
//! (`shep_client::Lagged`). A dashboard that only subscribed would miss exactly
//! the events load produces, which is exactly when a dashboard matters. The
//! answer is the one `crate::dog::bark::run_loop` already proved: subscribe AND
//! poll, and let a dropped or lagged frame trigger an immediate poll rather
//! than waiting for the scheduled one. The drop itself carries no information
//! about what was lost, so asking the shepherd what things look like now is the
//! only repair there is.
//!
//! **The freeze is Rin's ruling and it is not `bleats`' behaviour.** `bleats`
//! prints a notice and exits when its connection ends, which is right for a
//! follow. A standing dashboard that vanished would take the last known state
//! of the flock with it, at the moment an operator most wants to read it. So
//! this climbs [`RECONNECT_ATTEMPTS`] rungs and then sends [`Msg::Frozen`] and
//! ENDS — no more polls, no more dials, no more subscriptions. The UI loop
//! keeps running with the last values on screen until the operator quits.
//!
//! [`run_link`]'s real caller is `super::mod`'s `lookout`, which spawns it
//! alongside the UI loop.

use core::fmt;
use std::time::Duration;

use shep_client::{Lagged, RequestError};
use shep_core::protocol::BusEvent;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use super::app::Msg;
// `LinkError` is deliberately NOT imported: the error arm below never names
// the type, and an unused import fails `-D warnings`. The tests import it.
use super::source::{EventSource, FlockSource, Shepherd};

/// How often the flock is re-listed when nothing has gone wrong.
///
/// Two seconds. The pane's content already changes on a `process.*` event
/// within milliseconds; this exists to repair drift, not to animate. Two
/// seconds is inside an operator's own "is this thing live" patience while
/// being far cheaper than the per-frame polling a naive dashboard does. For
/// scale: the bark dog's fallback poll is 30s because nothing is watching it,
/// and the memory-limit sampler is 15s because it walks the process table — a
/// `ListFlock` is a map lookup and one frame each way.
pub const FLOCK_POLL: Duration = Duration::from_secs(2);

/// How many times the link re-dials before it gives up and freezes.
///
/// Five, at [`RECONNECT_FIRST_WAIT`] doubling to [`RECONNECT_MAX_WAIT`], is
/// 250 + 500 + 1000 + 2000 + 4000 ms — **7.75 seconds** of waiting. A shepherd
/// being restarted deliberately (`shep kill` then `shep muster`, or a systemd
/// restart) is back inside that window, so an operator watching through a
/// restart sees "reconnecting" and then recovery and never sees a freeze. A
/// shepherd that is genuinely gone is declared gone before that operator has
/// walked away from the terminal.
///
/// Thirty seconds would leave a dead dashboard claiming to be live for half a
/// minute. Two would flip to frozen during an ordinary restart and teach the
/// operator to distrust the banner.
pub const RECONNECT_ATTEMPTS: u32 = 5;

/// The wait before the first re-dial.
pub const RECONNECT_FIRST_WAIT: Duration = Duration::from_millis(250);

/// The ceiling on the doubling.
pub const RECONNECT_MAX_WAIT: Duration = Duration::from_secs(4);

/// The dashboard stopped listening: its [`Msg`] channel is closed.
///
/// The only condition [`run_connected`] reports that is not a reconnect —
/// every other failure it can meet is a rung on the ladder. A named type
/// rather than `()` for two reasons, one of them a gate: an exported
/// `Result<_, ()>` trips `clippy::result_unit_err`, which
/// `cargo clippy -- -D warnings` turns into a task-gate failure. The other is
/// that "the UI is gone" reads at a call site where `Err(())` does not.
///
/// No `#[non_exhaustive]`, for the same reason [`super::source::LinkError`]
/// carries none: IR-20's obligation is on `pub` error types in LIBRARY crates,
/// and shep-cli is `[[bin]]`-only. Said out loud rather than left silent,
/// which is the half of IR-20 that applies either way.
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
///
/// A two-field struct rather than a tuple: three signatures and an `# Errors`
/// section name it, and `Ok((polls, requests))` reads as nothing at any of
/// them.
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
/// **`opened` is handed in, already connected.** The first dial belongs to
/// `super::lookout`, which makes it before entering raw mode so that a
/// shepherd which was never running refuses the way every other client verb
/// refuses — `daemon_unreachable`, exit 5, no alternate screen — instead of
/// eight seconds of reconnect banner about a death that never happened. Rin's
/// retry-then-freeze ruling is about a shepherd that dies *underneath* a
/// running dashboard, and this signature is where the distinction is enforced
/// rather than described.
///
/// Ends — and only ends — after sending [`Msg::Frozen`], or when the UI stops
/// listening. Everything else it can encounter is a rung on the ladder.
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
                    // string would put a different sentence on screen every
                    // 250ms during an ordinary restart.
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

        // `attempt > 0`, NOT "a connection was opened once before". The gate
        // is on whether a `Retrying` was ANNOUNCED, because `Relinked` is what
        // takes that banner down again — and this is where an earlier draft
        // was wrong: a flag set only by a successful dial left the sequence
        // "first dial fails, second succeeds" showing
        // `reconnecting (attempt 1)` over a fully live dashboard, for the rest
        // of the session.
        if attempt > 0 && msgs.send(Msg::Relinked).await.is_err() {
            return;
        }
        attempt = 0;
        wait = RECONNECT_FIRST_WAIT;

        match run_connected(flock, events, msgs.clone(), channels, period).await {
            // The connection ended. `channels` comes back so the next rung can
            // hand it to the next connection — passed by value rather than by
            // `&mut` because `run_connected` is `tokio::spawn`ed directly by
            // its own tests, and a spawned future cannot borrow a caller's
            // local.
            Ok(returned) => channels = returned,
            // The UI is gone. Nothing left to report to.
            Err(UiGone) => return,
        }
    }
}

/// One connection's lifetime: an opening listing, then subscribe-and-poll
/// until the subscription ends.
///
/// Returns `channels` on `Ok`, so the next rung of the ladder can hand it to
/// the next connection.
///
/// # Errors
/// [`UiGone`] when the dashboard's own [`Msg`] channel has closed — the only
/// condition here that is not a reconnect. A failed poll, a dropped frame, a
/// lagging receiver and the subscription ending are all handled in place or
/// handed back to the ladder as `Ok`.
///
/// **Order matters, and it is the opposite of `bleats`'.** `bleats` lists
/// before it subscribes, because its id/name cache has to exist before the
/// first line arrives. lookout subscribes first: its rows carry a whole
/// `ProcessInfo`, so an event arriving before the first snapshot upserts into
/// an empty map perfectly well — while list-then-subscribe would lose every
/// event in the gap for no gain. `Shepherd::link` has already subscribed by the
/// time this is called; the listing below is the first thing that happens
/// after.
pub async fn run_connected<F: FlockSource, E: EventSource>(
    flock: F,
    mut events: E,
    msgs: mpsc::Sender<Msg>,
    mut channels: Channels,
    period: Duration,
) -> Result<Channels, UiGone> {
    // The opening listing. Every connection begins with one — a cold start and
    // a reconnect need the same first snapshot, and this is the one place that
    // serves both. Every poll count in this module's tests counts it.
    reconcile(&flock, &msgs).await?;

    // `interval_at`, not `interval`: a plain `tokio::time::interval` yields its
    // first tick immediately, which would make the first scheduled poll
    // unattributable — it would fire whether or not the interval had elapsed.
    // The opening listing above is the startup poll, deliberately and visibly.
    // Bark's own loop names the same trap.
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
            // would spin this loop at full tilt. The pattern not matching
            // disables the branch instead.
            Some(sent) = channels.requests.recv() => {
                // Awaited inline, which holds the other arms for the
                // request's duration. That is already this loop's established
                // behaviour: the poll arm awaits `reconcile` the same way,
                // bounded by the client's own deadline.
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
                    // `Dropped` is a real, named variant this binary
                    // understands, so it gets a repair as well as being
                    // forwarded — it must NOT be treated as an ordinary event.
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
/// A failed poll is dropped rather than propagated: the next event or the next
/// tick tries again, and one bad round trip must not take the connection down
/// — the same call bark's own `reconcile` makes. A poll that fails because the
/// connection is dead will be followed by the subscription ending, which is the
/// condition that does climb the ladder.
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
/// Formatted here rather than in `super::app`, which holds no clock and no
/// formatter — see [`Msg::Frozen`]'s own doc. Local, not UTC, for the same
/// reason `output::table::local_timestamp` is: this is read during an incident,
/// at a terminal, by someone thinking in wall-clock time.
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
    // Named only from here: `run_link`'s own error arm never spells the type.
    use crate::lookout::source::LinkError;

    fn sheep(id: u32) -> ProcessInfo {
        ProcessInfo::builder(id, format!("sheep-{id}"), ProcStatus::Online).build()
    }

    /// A flock source that counts what it was asked, so a test can assert on
    /// WHY a poll happened rather than only that one did.
    struct CountingFlock {
        polls: Arc<AtomicU64>,
    }

    impl FlockSource for CountingFlock {
        async fn flock(&self) -> Result<Vec<ProcessInfo>, RequestError> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![sheep(1)])
        }

        // Ignores its argument: the tests that use this fixture are about
        // poll counting and know nothing about requests.
        async fn send(&self, _request: Request) -> Result<Response, RequestError> {
            Ok(Response::Described(Vec::new()))
        }
    }

    /// A REAL `broadcast::Receiver` with a tiny capacity, so the bus genuinely
    /// drops frames. IR-33: hand-rolled, no mock crate. A fake that delivered
    /// everything would prove the fast path, which was never the risk.
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

    /// fails if a lagging subscriber stops triggering an immediate poll. This
    /// is the property the whole two-route design exists for: `broadcast`
    /// DROPS for a subscriber that falls behind, so a dashboard that only
    /// subscribed would go silently wrong under exactly the load that makes it
    /// worth watching. Bark's own reconciliation test is built the same way and
    /// for the same reason.
    ///
    /// IR-46: the wait on the message channel is bounded — a link task that
    /// never polls would otherwise hang this test rather than fail it.
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
            // A poll period far longer than this test, so ANY poll beyond the
            // opening listing is attributable to the lag and to nothing else.
            Duration::from_secs(3600),
        ));

        // The repair is what this test is about, so it waits for the SNAPSHOT
        // the repair produces rather than for the `BusLagged` notice that
        // precedes it: `run_connected` forwards the notice first and polls
        // second, so a test that stopped at the notice would read the counter
        // in a race with the poll it is counting.
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

    /// fails if a shepherd-side drop stops triggering a repair. `Dropped` is a
    /// real, named `BusEvent` this binary understands — it must NOT fall into
    /// the catch-all arm for variants a newer shepherd added.
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

        // The FIRST message on this channel is the opening listing's snapshot,
        // not the drop — every connection begins with one listing. Scanning
        // for the drop rather than asserting on message one is the difference
        // between testing this loop and testing bark's, which has no opening
        // listing and is where these fixtures came from.
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

    /// fails if the ladder stops being bounded, or stops ending in a freeze.
    /// Rin's ruling in one test: bounded retry, then a message saying the
    /// shepherd has died, then nothing. Never an exit.
    ///
    /// `start_paused` so the 250/500/1000/2000/4000 ms waits cost no wall
    /// clock; the `timeout` bound is real, because `run_link` failing to ever
    /// give up is exactly the regression this catches (IR-46).
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

        // `run_link` is HANDED its first connection rather than dialling for
        // it — the opening dial is `lookout::lookout`'s, before raw mode, so
        // that a shepherd which was never there refuses like every other verb
        // instead of freezing a dashboard about it. This opening connection
        // ends at once: its sender is dropped, so the subscription closes on
        // the first read and the ladder takes over.
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
        // RECONNECT_ATTEMPTS more behind the 250/500/1000/2000/4000 ms waits.
        // Only the delayed ones announce a `Retrying`, which is why the two
        // counts below differ by exactly one.
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

        // And it ENDS. A link task still alive after a freeze would keep a
        // dead connection's machinery running behind a screen that says it is
        // gone.
        let ended = tokio::time::timeout(Duration::from_secs(5), task).await;
        assert!(ended.is_ok(), "the link task ended after freezing");
    }

    /// fails if a reconnect that succeeds does not re-subscribe and re-list.
    /// A relink that reconnected but kept the old (dead) subscription would
    /// leave the dashboard live-looking and permanently stale — worse than the
    /// freeze, because nothing says so.
    #[tokio::test(start_paused = true)]
    async fn a_successful_relink_reports_live_on_the_first_success() {
        struct FailsOnce {
            done: bool,
            polls: Arc<AtomicU64>,
            /// Holds the reconnected subscription's SENDER open.
            ///
            /// Not decoration. An earlier version of this fixture built the
            /// channel with `let (_tx, rx)` and dropped the sender on the
            /// spot, so the "successful" relink handed back a stream that
            /// ended on its first read; the ladder went round a second time,
            /// and the `Relinked` this test observed came from that second
            /// cycle. It passed while the first relink announced nothing at
            /// all — which was the actual bug, and the one this test claims
            /// to be about.
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
        // `polls` below counts only what the RECONNECTED one listed.
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

    /// fails if the scheduled poll stops firing on its own schedule, or starts
    /// firing immediately at startup. The first poll must always be
    /// attributable — to the opening listing, to a drop, or to the interval
    /// genuinely elapsing — never to `tokio::time::interval`'s own quirk of
    /// yielding its first tick at once. Bark's loop names the same trap.
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
    /// answers each with one row. IR-33: hand-rolled, no mock crate.
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

    /// fails if a request never reaches the shepherd, or if its reply never
    /// comes back tagged with what it answered. The echo tag is what routes a
    /// reply with no correlation id, including an `Err` that carries no shape
    /// of its own.
    ///
    /// IR-46: the wait is bounded, so a loop that never sends hangs nothing.
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
