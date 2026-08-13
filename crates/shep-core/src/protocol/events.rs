//! Bus events broadcast to subscribed clients

use serde::{Deserialize, Serialize};

use crate::protocol::request::ProcessInfo;

/// What happened to a sheep
// wire format: changing existing variants is a breaking change
//
// A NEW variant is additive for Rust and for the protocol version, but it is
// not free for a subscriber that predates it, and this enum is the one place
// in the protocol where that is true. There is no `#[serde(other)]` fallback,
// and every variant's topic is `process.<something>`, which the `process.*`
// glob an existing subscriber already uses matches — so an older client is
// sent a frame it cannot decode. It drops that frame; it is not sent
// anything it asked for and lost. The same does not arise for `Request` or
// `Response`, where an old client never sends the verb whose answer it could
// not read. Weigh that cost against the alternative before adding one:
// reusing an existing kind and leaving subscribers to infer the event is the
// other option, and it was the losing one for reload only because a reload's
// reply is an acceptance, which leaves the bus as the only place its outcome
// is ever reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProcessEventKind {
    /// Spawn initiated
    Start,
    /// Became ready/online
    Online,
    /// Process exited
    Exit,
    /// Restart initiated
    Restart,
    /// A reload is replacing this instance: its replacement has been spawned
    /// into the same instance slot, and this one will be asked to go once
    /// that replacement is serving
    Reload,
    /// This instance has replaced the one it was spawned to drain, and that
    /// one is gone — the swap is over
    Reloaded,
    /// A reload gave up, so the instances it had not reached are left alone
    ///
    /// The instance named is the one the abandonment left holding the slot,
    /// and whether that one is serving depends on which abandonment it was.
    /// Where the reload gave up on replacing an instance, that instance is
    /// named and is still the app's live one. Where it gave up because the
    /// replacement went down instead, the replacement is named. As with every
    /// event here, `info` is that instance as it stood when the event was
    /// raised, so read `info.status` rather than assuming a live one.
    ReloadAbandoned,
    /// Stopped by request
    Stop,
    /// Deregistered
    Delete,
    /// Restart budget exhausted
    Errored,
}

/// One event on the daemon bus
///
/// Uses adjacently tagged serde format with `event` discriminator and `data` wrapper.
/// Subscription TOPICS are the dotted strings from [`BusEvent::topic`]
/// (`process.exit`, `log.out`, `daemon.*` — spec §6 grammar).
/// Phase 2's server-side filter globs against `topic()`.
// wire format: changing existing variants is a breaking change
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
// Adjacent tagging chosen because internally-tagged form cannot compile:
// the `Process` variant has its own `event: ProcessEventKind` field, which
// collides with an internal tag named `event` (serde_derive rejects this).
// Adjacently tagging (with `content = "data"`) avoids the collision while
// matching `Response`'s serde convention. Wire shape pinned by snapshot.
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum BusEvent {
    /// Lifecycle event for one sheep
    Process {
        /// What happened
        event: ProcessEventKind,
        /// Sheep snapshot at event time
        info: ProcessInfo,
        /// True when a user action caused it
        manually: bool,
        /// Unix millis
        at_ms: u64,
    },
    /// One stdout line from a sheep
    LogOut {
        /// Sheep id
        id: u32,
        /// The line (no trailing newline)
        line: String,
    },
    /// One stderr line from a sheep
    LogErr {
        /// Sheep id
        id: u32,
        /// The line
        line: String,
    },
    /// The bounded queue dropped this many events for this subscriber
    Dropped {
        /// Dropped-event count since last notice
        count: u64,
    },
    /// Daemon is shutting down
    DaemonShutdown,
}

impl BusEvent {
    /// The dotted subscription topic for this event (spec §6 grammar)
    #[must_use]
    pub fn topic(&self) -> &'static str {
        match self {
            Self::Process { event, .. } => match event {
                ProcessEventKind::Start => "process.start",
                ProcessEventKind::Online => "process.online",
                ProcessEventKind::Exit => "process.exit",
                ProcessEventKind::Restart => "process.restart",
                ProcessEventKind::Reload => "process.reload",
                ProcessEventKind::Reloaded => "process.reloaded",
                ProcessEventKind::ReloadAbandoned => "process.reload_abandoned",
                ProcessEventKind::Stop => "process.stop",
                ProcessEventKind::Delete => "process.delete",
                ProcessEventKind::Errored => "process.errored",
            },
            Self::LogOut { .. } => "log.out",
            Self::LogErr { .. } => "log.err",
            Self::Dropped { .. } => "daemon.dropped",
            Self::DaemonShutdown => "daemon.shutdown",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::request::ProcessInfo;
    use crate::status::ProcStatus;

    #[test]
    fn bus_event_wire_snapshots() {
        let mut events = vec![
            BusEvent::Process {
                event: ProcessEventKind::Exit,
                info: ProcessInfo {
                    id: 3,
                    name: "web".to_string(),
                    status: ProcStatus::WaitingRestart,
                    pid: None,
                    restarts: 2,
                    uptime_ms: 500,
                    fold: None,
                    out_file: Some("/home/rin/.shep/logs/web-0-out.log".to_string()),
                    err_file: Some("/home/rin/.shep/logs/web-0-err.log".to_string()),
                    // A bus event is built from the actor's own snapshot,
                    // which never carries a resource reading.
                    cpu_percent: None,
                    memory_bytes: None,
                    dog: None,
                },
                manually: false,
                at_ms: 1_700_000_000_000,
            },
            BusEvent::LogOut {
                id: 3,
                line: "listening on :8080".to_string(),
            },
            BusEvent::Dropped { count: 17 },
        ];

        // Every lifecycle kind a `process.*` subscriber can receive, over one
        // identical `info`, so the snapshot rows differ by their `event` tag
        // and by nothing else. Only `Exit` and the three reload kinds were
        // pinned before Phase 10; the six here are the ordinary events a real
        // integration — a dashboard, a bark rule — depends on first, and a
        // Rust-identifier rename on any of them would change the wire string
        // mechanically, compile clean, and break that integration silently.
        let sample = ProcessInfo::builder(3, "web", ProcStatus::WaitingRestart)
            .restarts(2)
            .uptime_ms(500)
            .out_file(Some("/home/rin/.shep/logs/web-0-out.log".to_string()))
            .err_file(Some("/home/rin/.shep/logs/web-0-err.log".to_string()))
            .build();

        let lifecycle = [
            ProcessEventKind::Start,
            ProcessEventKind::Online,
            ProcessEventKind::Restart,
            ProcessEventKind::Stop,
            ProcessEventKind::Delete,
            ProcessEventKind::Errored,
        ]
        .map(|event| BusEvent::Process {
            event,
            info: sample.clone(),
            manually: false,
            at_ms: 1_700_000_000_000,
        });

        events.extend(lifecycle);
        insta::assert_json_snapshot!("bus_event_wire_v1", events);
    }

    #[test]
    fn topics_follow_the_dotted_grammar() {
        // spec §6: process.* / log.out / log.err / daemon.*
        let e = BusEvent::LogOut {
            id: 1,
            line: String::new(),
        };
        assert_eq!(e.topic(), "log.out");
        assert_eq!(BusEvent::DaemonShutdown.topic(), "daemon.shutdown");
    }

    /// The three kinds a reload reports itself with, pinned as topic strings
    /// and as wire strings.
    ///
    /// Fails if [`BusEvent::topic`] maps any of them to the wrong dotted
    /// string — a typo there is invisible to a `process.*` subscriber, which
    /// matches anything under `process.`, and silently unreachable to one
    /// that named the topic it wanted. Fails too if a variant's serde
    /// spelling drifts from its snake_case default (a stray
    /// `#[serde(rename)]`, or a variant renamed without its topic): a reload's
    /// reply is an acceptance, so these frames are the whole of what a client
    /// ever learns about how the reload went, and a client matching on the
    /// wire string would stop recognising them.
    #[test]
    fn a_reload_reports_itself_under_three_topics() {
        for (kind, topic, wire) in [
            (ProcessEventKind::Reload, "process.reload", "\"reload\""),
            (
                ProcessEventKind::Reloaded,
                "process.reloaded",
                "\"reloaded\"",
            ),
            (
                ProcessEventKind::ReloadAbandoned,
                "process.reload_abandoned",
                "\"reload_abandoned\"",
            ),
        ] {
            let event = BusEvent::Process {
                event: kind,
                info: ProcessInfo {
                    id: 3,
                    name: "web".to_string(),
                    status: ProcStatus::Stopping,
                    pid: Some(4242),
                    restarts: 0,
                    uptime_ms: 0,
                    fold: None,
                    out_file: None,
                    err_file: None,
                    cpu_percent: None,
                    memory_bytes: None,
                    dog: None,
                },
                manually: true,
                at_ms: 0,
            };
            assert_eq!(event.topic(), topic, "{kind:?}");
            assert_eq!(serde_json::to_string(&kind).unwrap(), wire, "{kind:?}");
        }
    }

    #[test]
    fn v1_bus_event_fixture_still_deserializes() {
        // Adjacent-tagged shape pinned as a byte fixture (IR-35).
        let fixture = r#"{"event":"log_out","data":{"id":3,"line":"ready"}}"#;
        let ev: BusEvent = serde_json::from_str(fixture).unwrap();
        assert!(matches!(ev, BusEvent::LogOut { id: 3, .. }));
    }
}
