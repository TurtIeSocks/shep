//! The daemon's process-wide event bus.
//!
//! One [`tokio::sync::broadcast`] channel carries every [`BusEvent`] to
//! every subscriber; each connection compiles its `Subscribe` topic patterns
//! into a [`TopicFilter`] and gets its own [`spawn_forwarder`] task pumping
//! matching frames into that connection's write queue.

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
/// Every connection subscribes off the returned sender's `.subscribe()`;
/// the sender itself is held by the daemon so the bus outlives every
/// individual connection.
#[must_use]
pub fn new_bus() -> broadcast::Sender<BusEvent> {
    broadcast::channel(BUS_CAPACITY).0
}

/// Compiled server-side topic filter for one subscription
#[derive(Debug)]
pub struct TopicFilter {
    set: GlobSet,
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

    /// The source patterns this filter was compiled from.
    #[must_use]
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }
}

/// Error type returned from [`TopicFilter::new`]
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

/// What the forwarder should do with one receive result
enum Forwarded {
    Frame(Bytes),
    Skip,
    Stop,
}

// Pure: every forwarding decision lives here so the task itself has nothing
// left to test but plumbing.
fn step(received: Result<BusEvent, RecvError>, filter: &TopicFilter) -> Forwarded {
    let event = match received {
        Ok(event) if filter.matches(&event) => event,
        Ok(_) => return Forwarded::Skip,
        // Drop notices BYPASS the filter on purpose: a subscriber to
        // `process.*` still has to learn it lost events, and `daemon.dropped`
        // would otherwise be filtered out exactly when it matters most.
        Err(RecvError::Lagged(count)) => BusEvent::Dropped { count },
        Err(RecvError::Closed) => return Forwarded::Stop,
    };
    match encode_frame(&event) {
        Ok(bytes) => Forwarded::Frame(bytes),
        Err(err) => {
            tracing::warn!(%err, topic = event.topic(), "dropping an unencodable bus event");
            Forwarded::Skip
        }
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
pub fn spawn_forwarder(
    mut rx: broadcast::Receiver<BusEvent>,
    filter: TopicFilter,
    out: mpsc::Sender<Bytes>,
) -> JoinHandle<()> {
    // Cancel-safety: `recv` and `send` are both cancel-safe and are awaited
    // sequentially (no select!), so an aborted forwarder can lose at most the
    // frame in flight — which the subscriber is no longer there to read.
    tokio::spawn(async move {
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

    fn filter(patterns: &[&str]) -> TopicFilter {
        let owned: Vec<String> = patterns.iter().map(|p| (*p).to_string()).collect();
        TopicFilter::new(&owned).unwrap()
    }

    fn process_event(id: u32, event: ProcessEventKind) -> BusEvent {
        BusEvent::Process {
            event,
            info: ProcessInfo {
                id,
                name: format!("sheep-{id}"),
                status: ProcStatus::Online,
                pid: Some(1000 + id),
                restarts: 0,
                uptime_ms: 0,
                fold: None,
            },
            manually: false,
            at_ms: 0,
        }
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
        let (tx, mut rx) = tokio::sync::broadcast::channel(4);
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
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel(16);
        let handle = spawn_forwarder(rx, filter(&["process.*"]), out_tx);

        tx.send(process_event(0, ProcessEventKind::Start)).unwrap();
        tx.send(BusEvent::LogOut {
            id: 0,
            line: "noise".to_string(),
        })
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

    #[tokio::test(start_paused = true)]
    async fn forwarder_stops_when_the_subscriber_hangs_up() {
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        let (out_tx, out_rx) = tokio::sync::mpsc::channel(1);
        let handle = spawn_forwarder(rx, filter(&["*"]), out_tx);
        drop(out_rx);
        tx.send(BusEvent::DaemonShutdown).unwrap();
        handle.await.unwrap(); // resolves rather than leaking a task
    }
}
