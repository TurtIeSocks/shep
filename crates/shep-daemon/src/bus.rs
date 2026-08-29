//! The daemon's process-wide event bus.
//!
//! One [`tokio::sync::broadcast`] channel carries every [`SharedEvent`] to
//! every subscriber; each connection compiles its `Subscribe` topic patterns
//! into a [`TopicFilter`] and gets its own [`spawn_forwarder`] task pumping
//! matching frames into that connection's write queue.

use core::ops::Deref;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use globset::{Glob, GlobSet, GlobSetBuilder};
use shep_core::protocol::{BusEvent, encode_frame};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Ring capacity of the daemon event bus.
///
/// Every subscriber reads this one ring at its own cursor, so the number is
/// the per-subscriber backlog: a subscriber more than `BUS_CAPACITY` events
/// behind loses the OLDEST ones and is told how many (spec §6's
/// drop-oldest-plus-`Dropped`-notice rule). 1024 events is ~1 MiB at a 1 KiB
/// log line — enough that a client stalled for a second on a chatty sheep
/// catches up, small enough to leave the single-digit-MB idle footprint goal
/// alone (spec §14.11).
pub const BUS_CAPACITY: usize = 1024;

/// Ceiling on topic patterns per `Subscribe`.
///
/// Patterns are peer-supplied and each compiles into the connection's
/// matcher; bounding the count bounds that work the same way the selector's
/// regex size limit bounds a compiled pattern.
pub const MAX_TOPIC_PATTERNS: usize = 32;

/// Creates the daemon's process-wide event bus.
///
/// Every connection subscribes off the returned [`Bus`]; the bus itself is
/// held by the daemon so it outlives every individual connection.
#[must_use]
pub fn new_bus() -> Bus {
    Bus {
        tx: broadcast::channel(BUS_CAPACITY).0,
        log_subscribers: Arc::new(AtomicUsize::new(0)),
    }
}

/// The daemon's event channel, plus the one question the channel cannot
/// answer: whether anything is listening for log lines.
///
/// A `broadcast::Sender`'s `receiver_count` is the wrong question here and
/// would always answer "yes". Two subscribers exist for the daemon's whole
/// life and neither reads a log line: [`crate::dogs::spawn_dog_watch`] acts
/// only on a dog's `Errored`, and the snapshot writer only on a lifecycle
/// change. A sheep's stdout therefore woke both of them, once per line, to
/// look at an event and drop it — measured at 39% of the daemon's per-line
/// CPU on a sheep emitting 7,315 lines/s with nothing attached.
///
/// So the count kept here is of subscribers whose [`TopicFilter`] actually
/// matches a log topic, which in practice means a `shep bleats`, a bark dog,
/// or a `lookout`, and [`Self::publish_log`] skips the whole publish while it
/// is zero.
///
/// # Debug (IR-41)
///
/// Derived, unredacted, and carrying nothing to redact: a channel handle and
/// a counter.
#[derive(Clone, Debug)]
pub struct Bus {
    tx: broadcast::Sender<SharedEvent>,
    /// Subscribers whose filter matches `log.out` or `log.err`.
    log_subscribers: Arc<AtomicUsize>,
}

impl Bus {
    /// Publishes one log line, unless nothing is subscribed to log topics.
    ///
    /// # Why dropping it is not a lost event
    ///
    /// A `broadcast` receiver begins at the channel's CURRENT tail
    /// (`tokio::sync::broadcast`'s `new_receiver` reads `tail.pos` into the
    /// receiver's cursor), so a subscriber has never been shown an event
    /// published before it attached. An event skipped while the count is zero
    /// is therefore one no receiver could have read had it been published:
    /// the ring would have carried it, every existing subscriber would have
    /// filtered it out, and the next one to attach would have started past
    /// it.
    ///
    /// What the gate does widen, by the time it takes one atomic store to
    /// become visible to another core, is a window that is already there:
    /// [`Self::subscribe_for`] registers a filter's interest BEFORE it takes
    /// its receiver, so between an operator running `shep bleats` and its
    /// first line there has always been an interval in which a line goes to
    /// nobody. Nothing orders a client's `Subscribe` against a sheep's
    /// stdout, and nothing could.
    pub fn publish_log(&self, event: BusEvent) {
        // Relaxed: nothing is published THROUGH this counter, and the only
        // consequence of reading a stale zero is the race above.
        if self.log_subscribers.load(Ordering::Relaxed) == 0 {
            return;
        }
        let _ = self.tx.send(SharedEvent::new(event));
    }

    /// A receiver for one subscription, plus its registered log interest.
    ///
    /// Registered before the receiver exists, so the receiver can only ever
    /// miss events from before it was created — see [`Self::publish_log`].
    /// The guard restores the count when it is dropped, which for a
    /// forwarder is when its task ends OR is aborted, since aborting a task
    /// drops the future and everything it captured.
    #[must_use]
    fn subscribe_for(
        &self,
        filter: &TopicFilter,
    ) -> (broadcast::Receiver<SharedEvent>, LogInterest) {
        let interest = if filter.wants_logs() {
            self.log_subscribers.fetch_add(1, Ordering::Relaxed);
            LogInterest(Some(Arc::clone(&self.log_subscribers)))
        } else {
            LogInterest(None)
        };
        (self.tx.subscribe(), interest)
    }

    /// How many subscribers currently want log topics.
    #[cfg(test)]
    fn log_subscribers(&self) -> usize {
        self.log_subscribers.load(Ordering::Relaxed)
    }
}

impl Deref for Bus {
    type Target = broadcast::Sender<SharedEvent>;

    // IR-25: a field return, on the path every non-log publish takes.
    #[inline]
    fn deref(&self) -> &broadcast::Sender<SharedEvent> {
        &self.tx
    }
}

/// One subscriber's registered interest in log topics, released on drop.
///
/// `None` for a filter that never wanted logs, so a caller holds the same
/// type either way and nothing branches on the way out.
#[derive(Debug)]
pub struct LogInterest(Option<Arc<AtomicUsize>>);

impl Drop for LogInterest {
    fn drop(&mut self) {
        if let Some(count) = self.0.take() {
            count.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// A bus of the given ring capacity, plus one plain receiver.
///
/// The receiver is not registered for log topics, and cannot be: a bare
/// `subscribe` has no [`TopicFilter`] for [`Bus::publish_log`] to ask about.
/// A test that wants log lines takes them through [`spawn_forwarder`], the
/// way a connection does.
#[cfg(test)]
pub(crate) fn test_bus(capacity: usize) -> (Bus, broadcast::Receiver<SharedEvent>) {
    let (tx, rx) = broadcast::channel(capacity);
    let bus = Bus {
        tx,
        log_subscribers: Arc::new(AtomicUsize::new(0)),
    };
    (bus, rx)
}

/// One published [`BusEvent`], plus the wire frame its subscribers share.
///
/// The bus carries this rather than the event itself because both of the
/// costs it removes are LINEAR IN SUBSCRIBERS. A `broadcast` channel clones
/// its item once per receiver, which for a `LogOut` is a fresh copy of the
/// line; and every forwarder used to encode its own frame from that copy,
/// so the same bytes were built once per attached client. Measured on a
/// sheep logging 7,315 lines/s: one attached `shep bleats` took the daemon
/// from 23.99% of a core to 37.50%, +18.6 us per line. Here a clone is a
/// refcount bump and the frame is built by whichever forwarder asks first.
///
/// The bytes are unchanged — this is how OFTEN [`encode_frame`] runs, never
/// what it produces.
///
/// # Debug (IR-41)
///
/// Unredacted, and hand-written rather than derived. Nothing here is a
/// secret that [`BusEvent`]'s own `Debug` is not already trusted with — the
/// wrapper adds a cached frame, which is that same event encoded, and a
/// test-only encode tally. Both are dropped from the output: the frame
/// because printing a payload twice tells a reader nothing, and the tally
/// because a `#[cfg(test)]` field in a derived format would make the string
/// this type prints under `cargo test` a different one from the string it
/// prints in a release daemon.
#[derive(Clone)]
pub struct SharedEvent(Arc<Shared>);

impl core::fmt::Debug for SharedEvent {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SharedEvent")
            .field("event", &self.0.event)
            .field("encoded", &self.0.frame.get().is_some())
            .finish()
    }
}

/// [`SharedEvent`]'s payload — one allocation per published event.
#[derive(Debug)]
struct Shared {
    event: BusEvent,
    /// The wire frame, filled by the first forwarder that needs it.
    ///
    /// `Some(None)` once encoding has been TRIED and failed, so an event
    /// nothing can encode is reported once rather than once per subscriber.
    frame: OnceLock<Option<Bytes>>,
    /// How many times [`encode_frame`] has actually run for this event.
    ///
    /// Test-only. [`OnceLock`] already makes "at most once" a fact about the
    /// type; what a test cannot otherwise see is that the encode ran AT ALL
    /// through this path, and that no future edit reintroduces a
    /// per-subscriber one beside it.
    #[cfg(test)]
    encodes: AtomicUsize,
}

impl SharedEvent {
    /// Wraps one event for publication on the bus.
    #[must_use]
    pub fn new(event: BusEvent) -> Self {
        Self(Arc::new(Shared {
            event,
            frame: OnceLock::new(),
            #[cfg(test)]
            encodes: AtomicUsize::new(0),
        }))
    }

    /// How many times this event has been through [`encode_frame`].
    #[cfg(test)]
    fn encodes(&self) -> usize {
        self.0.encodes.load(Ordering::Relaxed)
    }

    /// A clone of the event this carries, for a caller that needs to own one.
    ///
    /// Everything on the daemon's own hot paths reads through [`Deref`]
    /// instead; this is for a caller that has to destructure a received
    /// event by value.
    #[must_use]
    pub fn to_event(&self) -> BusEvent {
        self.0.event.clone()
    }

    /// This event's wire frame, encoded on the first call and shared after.
    ///
    /// `None` when the event cannot be encoded at all, which is warned about
    /// exactly once and leaves every subscriber skipping it alike.
    fn frame(&self) -> Option<&Bytes> {
        self.0
            .frame
            .get_or_init(|| {
                #[cfg(test)]
                self.0.encodes.fetch_add(1, Ordering::Relaxed);
                match encode_frame(&self.0.event) {
                    Ok(bytes) => Some(bytes),
                    Err(err) => {
                        tracing::warn!(
                            %err,
                            topic = self.0.event.topic(),
                            "dropping an unencodable bus event"
                        );
                        None
                    }
                }
            })
            .as_ref()
    }
}

impl Deref for SharedEvent {
    type Target = BusEvent;

    // IR-25: a field return through one pointer hop, on the path every
    // subscriber takes for every event.
    #[inline]
    fn deref(&self) -> &BusEvent {
        &self.0.event
    }
}

impl From<BusEvent> for SharedEvent {
    fn from(event: BusEvent) -> Self {
        Self::new(event)
    }
}

/// Compares a published event against the event it was published from, so a
/// caller reading the bus asserts on what it sent rather than on the wrapper.
impl PartialEq<BusEvent> for SharedEvent {
    fn eq(&self, other: &BusEvent) -> bool {
        self.0.event == *other
    }
}

/// Compiled server-side topic filter for one subscription
#[derive(Debug)]
pub struct TopicFilter {
    set: GlobSet,
    /// Read only by [`Self::patterns`], which only this crate's own tests
    /// call, so in a crate-private module both are dead in a non-test build.
    /// `allow` rather than `expect` because the expectation would go
    /// unfulfilled in the test build, where the tests do call it.
    #[allow(
        dead_code,
        reason = "read by this crate's own tests through `patterns`"
    )]
    patterns: Vec<String>,
}

impl TopicFilter {
    /// Compiles one subscription's topic patterns into a matcher.
    ///
    /// # Errors
    /// - [`BusError::TooManyPatterns`] — more than [`MAX_TOPIC_PATTERNS`].
    /// - [`BusError::BadPattern`] — a pattern the glob compiler rejects.
    pub fn new(patterns: &[String]) -> Result<Self, BusError> {
        if patterns.len() > MAX_TOPIC_PATTERNS {
            return Err(BusError::TooManyPatterns(patterns.len()));
        }
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            let glob = Glob::new(pattern).map_err(|e| BusError::BadPattern {
                pattern: pattern.clone(),
                message: e.to_string(),
            })?;
            builder.add(glob);
        }
        let set = builder.build().map_err(|e| BusError::BadPattern {
            pattern: patterns.join(", "),
            message: e.to_string(),
        })?;
        Ok(Self {
            set,
            patterns: patterns.to_vec(),
        })
    }

    /// True when `event`'s [`BusEvent::topic`] matches one of this filter's patterns.
    #[must_use]
    pub fn matches(&self, event: &BusEvent) -> bool {
        self.set.is_match(event.topic())
    }

    /// True when this filter would match either log topic.
    ///
    /// Asked once per subscription, never per event, and the answer is what
    /// [`Bus::publish_log`] gates on. The two topic strings are duplicated
    /// from [`BusEvent::topic`] because there is no event to ask — a
    /// publisher decides whether to BUILD one. `wants_logs_agrees_with_the
    /// _topics_log_events_carry` is what keeps the two in step.
    #[must_use]
    pub fn wants_logs(&self) -> bool {
        ["log.out", "log.err"]
            .iter()
            .any(|topic| self.set.is_match(topic))
    }

    /// The source patterns this filter was compiled from.
    // IR-25: trivial field return, no branch — inline across codegen units.
    // Not per-frame hot like `matches` above (a `GlobSet` call, not a
    // forwarding one), so `#[inline]`, never `#[inline(always)]`.
    #[inline]
    #[must_use]
    #[allow(dead_code, reason = "called by this crate's own tests")]
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }
}

/// Error type returned from [`TopicFilter::new`]
///
/// `#[non_exhaustive]`: today's two variants cover a pattern that will not
/// compile and a subscribe that asked for too many, and a future
/// subscribe-time check — a wildcard-depth limit, or a reserved-topic
/// refusal — would need its own variant rather than stretching
/// [`Self::BadPattern`] to cover a rule the compiler never rejected, and
/// shep-daemon is a published library an out-of-tree matcher should not
/// break for (IR-20).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusError {
    /// A subscribe pattern failed to compile (carries pattern + compiler message)
    BadPattern {
        /// The rejected pattern
        pattern: String,
        /// The glob compiler's error message
        message: String,
    },
    /// More than [`MAX_TOPIC_PATTERNS`] patterns in one subscribe (carries the count)
    TooManyPatterns(usize),
}

impl core::fmt::Display for BusError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadPattern { pattern, message } => {
                write!(f, "invalid topic pattern `{pattern}`: {message}")
            }
            Self::TooManyPatterns(count) => {
                write!(
                    f,
                    "{count} topic patterns exceeds the limit of {MAX_TOPIC_PATTERNS}"
                )
            }
        }
    }
}

impl core::error::Error for BusError {}

/// What the forwarder should do with one receive result
enum Forwarded {
    Frame(Bytes),
    Skip,
    Stop,
}

// Pure: every forwarding decision lives here so the task itself has nothing
// left to test but plumbing.
fn step(received: Result<SharedEvent, RecvError>, filter: &TopicFilter) -> Forwarded {
    let event = match received {
        Ok(event) if filter.matches(&event) => event,
        Ok(_) => return Forwarded::Skip,
        // Drop notices BYPASS the filter on purpose: a subscriber to
        // `process.*` still has to learn it lost events, and `daemon.dropped`
        // would otherwise be filtered out exactly when it matters most.
        //
        // Wrapped like any other event, but the count is this subscriber's
        // own, so this is the one frame per lag nothing else can share.
        Err(RecvError::Lagged(count)) => SharedEvent::new(BusEvent::Dropped { count }),
        Err(RecvError::Closed) => return Forwarded::Stop,
    };
    match event.frame() {
        Some(bytes) => Forwarded::Frame(bytes.clone()),
        None => Forwarded::Skip,
    }
}

/// Spawns the forward task for one subscriber
///
// Back-pressure model (IR-31): a client that stops reading stalls the
// connection's writer, which fills the write queue, which parks this task on
// `send`, which stops draining the ring — so the *broadcast channel itself*
// becomes the bounded per-subscriber queue, drops the oldest events, and
// reports the exact count as `Lagged(n)`. That is spec §6's requirement
// implemented by the runtime rather than by a hand-rolled `VecDeque`, and it
// isolates one slow client from every other connection.
pub fn spawn_forwarder(bus: &Bus, filter: TopicFilter, out: mpsc::Sender<Bytes>) -> JoinHandle<()> {
    let (mut rx, interest) = bus.subscribe_for(&filter);
    // Cancel-safety: `recv` and `send` are both cancel-safe and are awaited
    // sequentially (no select!), so an aborted forwarder can lose at most the
    // frame in flight — which the subscriber is no longer there to read.
    tokio::spawn(async move {
        // Captured by the future rather than created inside it, so a
        // forwarder aborted before its first poll still releases the count.
        let _interest = interest;
        loop {
            match step(rx.recv().await, &filter) {
                Forwarded::Frame(bytes) => {
                    if out.send(bytes).await.is_err() {
                        break; // subscriber hung up
                    }
                }
                Forwarded::Skip => {}
                Forwarded::Stop => break,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use shep_core::protocol::{BusEvent, ProcessEventKind, ProcessInfo, decode_frame};
    use shep_core::status::ProcStatus;

    /// How long a test waits for a forwarder to produce a frame.
    ///
    /// Virtual, not wall clock: every caller runs `start_paused`, so a
    /// forwarder that never sends leaves the runtime idle, time jumps
    /// straight here, and the test fails with its own message instead of
    /// hanging until CI's timeout (IR-33).
    const FORWARD_DEADLINE: core::time::Duration = core::time::Duration::from_secs(1);

    /// One frame from a forwarder, or a named failure if none arrives.
    async fn next_frame(out: &mut mpsc::Receiver<Bytes>) -> Bytes {
        tokio::time::timeout(FORWARD_DEADLINE, out.recv())
            .await
            .expect("a forwarder must produce a frame rather than stall")
            .expect("every subscriber must receive the event")
    }

    fn filter(patterns: &[&str]) -> TopicFilter {
        let owned: Vec<String> = patterns.iter().map(|p| (*p).to_string()).collect();
        TopicFilter::new(&owned).unwrap()
    }

    fn process_event(id: u32, event: ProcessEventKind) -> SharedEvent {
        SharedEvent::new(BusEvent::Process {
            event,
            info: ProcessInfo::builder(id, format!("sheep-{id}"), ProcStatus::Online)
                .pid(Some(1000 + id))
                .out_file(Some(format!("/logs/sheep-{id}-0-out.log")))
                .err_file(Some(format!("/logs/sheep-{id}-0-err.log")))
                .build(),
            manually: false,
            at_ms: 0,
        })
    }

    #[test]
    fn globs_match_the_dotted_topic_grammar() {
        let processes = filter(&["process.*"]);
        assert!(processes.matches(&process_event(0, ProcessEventKind::Exit)));
        assert!(!processes.matches(&BusEvent::LogOut {
            id: 0,
            line: String::new()
        }));
        let logs = filter(&["log.out", "log.err"]);
        assert!(logs.matches(&BusEvent::LogOut {
            id: 0,
            line: String::new()
        }));
        assert!(logs.matches(&BusEvent::LogErr {
            id: 0,
            line: String::new()
        }));
        assert!(!logs.matches(&BusEvent::DaemonShutdown));
        let everything = filter(&["*"]);
        assert!(everything.matches(&BusEvent::DaemonShutdown));
        assert!(everything.matches(&process_event(1, ProcessEventKind::Start)));
    }

    #[test]
    fn an_empty_topic_list_matches_nothing() {
        // Documented contract: subscribe to `*` for everything; an empty list
        // is a subscription to nothing, not a wildcard.
        let none = TopicFilter::new(&[]).unwrap();
        assert!(!none.matches(&BusEvent::DaemonShutdown));
        assert!(none.patterns().is_empty());
    }

    #[test]
    fn a_bad_pattern_is_a_typed_error_carrying_the_pattern() {
        let err = TopicFilter::new(&["process.[".to_string()]).unwrap_err();
        assert!(matches!(err, BusError::BadPattern { ref pattern, .. } if pattern == "process.["));
    }

    #[test]
    fn too_many_patterns_are_refused_with_the_count() {
        let many: Vec<String> = (0..=MAX_TOPIC_PATTERNS).map(|i| format!("t{i}")).collect();
        assert_eq!(
            TopicFilter::new(&many).unwrap_err(),
            BusError::TooManyPatterns(MAX_TOPIC_PATTERNS + 1)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn lag_becomes_a_dropped_notice_that_bypasses_the_filter() {
        // The count is READ from the runtime, never hand-computed: whatever
        // tokio says was missed is exactly what the subscriber is told.
        let (tx, mut rx) = test_bus(4);
        for id in 0..10 {
            tx.send(process_event(id, ProcessEventKind::Start)).unwrap();
        }
        let missed = match rx.recv().await {
            Err(RecvError::Lagged(n)) => n,
            other => panic!("expected a lag after overflowing the ring, got {other:?}"),
        };
        assert!(missed > 0);
        // `process.*` would filter a daemon.dropped topic out; it must not.
        let Forwarded::Frame(bytes) = step(Err(RecvError::Lagged(missed)), &filter(&["process.*"]))
        else {
            panic!("a lag must always produce a Dropped frame")
        };
        assert_eq!(
            decode_frame::<BusEvent>(&bytes).unwrap(),
            BusEvent::Dropped { count: missed }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn forwarder_delivers_only_matching_frames_then_closes_with_the_bus() {
        let (tx, _rx) = test_bus(16);
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(16);
        let handle = spawn_forwarder(&tx, filter(&["process.*"]), out_tx);

        tx.send(process_event(0, ProcessEventKind::Start)).unwrap();
        tx.send(
            BusEvent::LogOut {
                id: 0,
                line: "noise".to_string(),
            }
            .into(),
        )
        .unwrap();
        tx.send(process_event(0, ProcessEventKind::Online)).unwrap();
        drop(tx);
        handle.await.unwrap();

        // Ordering IS the filtering assertion: the two process frames arrive
        // back to back, so nothing was emitted for the log line between them.
        let first: BusEvent = decode_frame(&out_rx.recv().await.unwrap()).unwrap();
        let second: BusEvent = decode_frame(&out_rx.recv().await.unwrap()).unwrap();
        assert!(matches!(
            first,
            BusEvent::Process {
                event: ProcessEventKind::Start,
                ..
            }
        ));
        assert!(matches!(
            second,
            BusEvent::Process {
                event: ProcessEventKind::Online,
                ..
            }
        ));
        assert!(
            out_rx.recv().await.is_none(),
            "forwarder must drop its sender at bus close"
        );
    }

    /// Fails if every subscriber encodes its own copy of the same event.
    ///
    /// That cost is linear in attached clients, and it is the measured one:
    /// a sheep logging 7,315 lines/s cost the daemon 23.99% of a core with
    /// nobody watching and 37.50% with ONE `shep bleats` attached.
    ///
    /// Two assertions, and the second is the one that matters here. Byte
    /// equality is the wire contract, which a per-subscriber encode would
    /// also satisfy; SAME-BUFFER identity is what says the encode ran once
    /// and the rest of the subscribers cloned a refcount.
    #[tokio::test(start_paused = true)]
    async fn every_subscriber_gets_the_same_frame_from_one_encode() {
        let (tx, _keep) = test_bus(16);
        let mut outs = Vec::new();
        let mut forwarders = Vec::new();
        for _ in 0..3 {
            let (out_tx, out_rx) = tokio::sync::mpsc::channel(16);
            forwarders.push(spawn_forwarder(&tx, filter(&["*"]), out_tx));
            outs.push(out_rx);
        }

        let published = process_event(7, ProcessEventKind::Online);
        tx.send(published.clone()).unwrap();
        drop(tx); // buffered events are delivered before the close is seen

        let mut frames = Vec::new();
        for out in &mut outs {
            frames.push(next_frame(out).await);
        }
        for forwarder in forwarders {
            forwarder.await.unwrap();
        }

        let (first, rest) = frames
            .split_first()
            .expect("three subscribers, three frames");
        assert_eq!(
            decode_frame::<BusEvent>(first).unwrap(),
            published.to_event(),
            "sharing the frame must not change what is on the wire"
        );
        for frame in rest {
            assert_eq!(frame, first, "every subscriber must see identical bytes");
            assert_eq!(
                frame.as_ptr(),
                first.as_ptr(),
                "identical bytes are not enough: they must be the SAME buffer, \
                 or the daemon encoded this event once per subscriber"
            );
        }
    }

    /// Fails if [`encode_frame`] runs once per subscriber rather than once
    /// per event.
    ///
    /// A count rather than a clock: the cost is linear in attached clients,
    /// which is a fact about how many times a function ran and about nothing
    /// else. Zero before anyone asks is half the claim — a `SharedEvent`
    /// that eagerly encoded would charge every publisher for subscribers
    /// that may not exist.
    #[tokio::test(start_paused = true)]
    async fn encode_frame_runs_once_per_event_however_many_subscribers() {
        const SUBSCRIBERS: usize = 5;
        let (tx, _keep) = test_bus(16);
        let mut outs = Vec::new();
        let mut forwarders = Vec::new();
        for _ in 0..SUBSCRIBERS {
            let (out_tx, out_rx) = tokio::sync::mpsc::channel(16);
            forwarders.push(spawn_forwarder(&tx, filter(&["*"]), out_tx));
            outs.push(out_rx);
        }

        let published = process_event(11, ProcessEventKind::Start);
        assert_eq!(
            published.encodes(),
            0,
            "publishing must not encode before a subscriber needs the bytes"
        );

        tx.send(published.clone()).unwrap();
        drop(tx);

        let mut frames = Vec::new();
        for out in &mut outs {
            frames.push(next_frame(out).await);
        }
        for forwarder in forwarders {
            forwarder.await.unwrap();
        }

        assert_eq!(
            published.encodes(),
            1,
            "{SUBSCRIBERS} subscribers, one encode"
        );
        assert_eq!(frames.len(), SUBSCRIBERS);
        let (first, rest) = frames.split_first().expect("one frame per subscriber");
        for frame in rest {
            assert_eq!(frame, first, "every subscriber must see identical bytes");
        }
    }

    /// Fails if [`SharedEvent`]'s `Debug` starts carrying the encoded frame,
    /// the test-only encode tally, or a redaction nobody asked for (IR-41).
    ///
    /// An exact string rather than a `contains`: the claim is what a reader
    /// of a `tracing` line or a panic message SEES, and every part of it —
    /// the wrapper's name, the two fields, the event printed whole and
    /// unredacted — is a decision this pins.
    #[test]
    fn shared_event_debug_prints_the_event_and_whether_it_is_encoded() {
        let event = SharedEvent::new(BusEvent::LogOut {
            id: 3,
            line: "hello".to_string(),
        });
        assert_eq!(
            format!("{event:?}"),
            r#"SharedEvent { event: LogOut { id: 3, line: "hello" }, encoded: false }"#
        );
        event.frame().expect("a LogOut encodes");
        assert_eq!(
            format!("{event:?}"),
            r#"SharedEvent { event: LogOut { id: 3, line: "hello" }, encoded: true }"#
        );
    }

    #[tokio::test(start_paused = true)]
    async fn forwarder_stops_when_the_subscriber_hangs_up() {
        let (tx, _rx) = test_bus(16);
        let (out_tx, out_rx) = tokio::sync::mpsc::channel(1);
        let handle = spawn_forwarder(&tx, filter(&["*"]), out_tx);
        drop(out_rx);
        tx.send(BusEvent::DaemonShutdown.into()).unwrap();
        handle.await.unwrap(); // resolves rather than leaking a task
    }

    fn log_out(line: &str) -> BusEvent {
        BusEvent::LogOut {
            id: 1,
            line: line.to_string(),
        }
    }

    /// Fails if [`TopicFilter::wants_logs`]'s two hard-coded topics drift
    /// from the ones [`BusEvent::topic`] actually returns.
    ///
    /// `wants_logs` is asked without an event in hand, so it cannot call
    /// `topic()`; this is the join that keeps the duplication honest. A
    /// rename of either topic string fails here rather than silently making
    /// every `shep bleats` a subscriber to nothing.
    #[test]
    fn wants_logs_agrees_with_the_topics_log_events_carry() {
        let out = log_out("x");
        let err = BusEvent::LogErr {
            id: 1,
            line: "x".to_string(),
        };
        for patterns in [vec!["*"], vec!["log.*"], vec!["log.out", "log.err"]] {
            let f = filter(&patterns);
            assert!(f.wants_logs(), "{patterns:?} must register log interest");
            assert!(f.matches(&out) && f.matches(&err), "{patterns:?}");
        }
        let f = filter(&["process.*", "channel.*", "daemon.*"]);
        assert!(!f.wants_logs(), "no pattern here names a log topic");
        assert!(!f.matches(&out) && !f.matches(&err));
    }

    /// Fails if a log line is published while nothing wants one.
    ///
    /// The count is the assertion rather than a clock, because the cost being
    /// removed is a fact about how often a publish happens. `receiver_count`
    /// cannot stand in: the daemon always has two subscribers that read every
    /// event and act on no log line, and it is exactly their per-line wakeup
    /// that the gate exists to stop.
    #[tokio::test(start_paused = true)]
    async fn a_log_line_is_not_published_while_no_filter_wants_one() {
        let (bus, mut plain) = test_bus(16);
        assert_eq!(bus.log_subscribers(), 0);

        // A subscriber that reads everything and asked for nothing — the
        // shape both of the daemon's own internal subscribers have.
        bus.publish_log(log_out("dropped"));
        assert!(
            plain.try_recv().is_err(),
            "a log line must not reach the ring while no filter wants one"
        );

        // A non-log publish still goes out: the gate is about log topics,
        // not about publishing.
        bus.send(BusEvent::DaemonShutdown.into()).unwrap();
        assert_eq!(plain.try_recv().unwrap(), BusEvent::DaemonShutdown);
    }

    /// Fails if a subscriber that asked for log topics does not get them, or
    /// if the interest outlives the forwarder that registered it.
    #[tokio::test(start_paused = true)]
    async fn a_log_subscriber_opens_the_gate_and_closes_it_again() {
        let (bus, mut plain) = test_bus(16);
        let (out_tx, mut out_rx) = mpsc::channel(16);
        let forwarder = spawn_forwarder(&bus, filter(&["log.*"]), out_tx);
        assert_eq!(bus.log_subscribers(), 1, "a log.* filter must register");

        bus.publish_log(log_out("delivered"));
        assert_eq!(
            decode_frame::<BusEvent>(&next_frame(&mut out_rx).await).unwrap(),
            log_out("delivered")
        );
        assert_eq!(
            plain.try_recv().unwrap(),
            log_out("delivered"),
            "one open gate publishes to every subscriber, not only the asker"
        );

        forwarder.abort();
        let _ = forwarder.await;
        assert_eq!(
            bus.log_subscribers(),
            0,
            "an aborted forwarder must release its interest, not strand it"
        );
        bus.publish_log(log_out("dropped again"));
        assert!(plain.try_recv().is_err());
    }
}
