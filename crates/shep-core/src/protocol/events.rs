//! Bus events broadcast to subscribed clients

use serde::{Deserialize, Serialize};

use crate::protocol::channel::ChildMessage;
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
    /// One message a sheep wrote on its shepherd channel (fd 3).
    ///
    /// Child->shepherd only. The shepherd's own writes — the
    /// `{"kind":"shutdown"}` of `shutdown_with_message`, and an `action` a
    /// `Trigger` dispatched — are deliberately not here. Every one of them is
    /// something an operator or the daemon just did and already has a
    /// reporter: a shutdown message is followed by `process.stop`, and an
    /// action is answered to the caller that sent it by
    /// `Response::Triggered`. Putting them here as well would make this the
    /// only event on the bus reporting a REQUEST rather than an outcome, and
    /// would loop a dog that both subscribes and triggers back onto its own
    /// dispatches. Adding the outbound half later stays additive — another
    /// variant, more `channel.` topics, no version bump — so this is a
    /// narrowing, not a door closed.
    ///
    /// `message` is the app's own text, whole and unredacted, unlike the
    /// `[dog.<name>]` config that travels as [`DogSectionToml`]. Nothing on
    /// this wire is a credential: `Ready` is empty, `Metric` is a name and a
    /// float, and an `ActionReply` body is text the app chose to publish to
    /// whoever triggered it. That is what makes a derived `Debug` safe here.
    ///
    /// [`DogSectionToml`]: crate::protocol::DogSectionToml
    Channel {
        /// The sheep that wrote it.
        id: u32,
        /// The message, exactly as it came off fd 3.
        message: ChildMessage,
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
            // Total over `ChildMessage`, with no wildcard, and that is the
            // point of leaving that enum exhaustive (see its module doc): a
            // fourth kind on fd 3 fails to compile here until someone decides
            // what its topic is, rather than defaulting into a topic no
            // subscriber ever asked for.
            Self::Channel { message, .. } => match message {
                ChildMessage::Ready => "channel.ready",
                ChildMessage::Metric { .. } => "channel.metric",
                ChildMessage::ActionReply { .. } => "channel.action_reply",
            },
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

        // All three shepherd-channel topics, over one sheep id, because the
        // adjacent-tagged shape puts the message's own `kind` INSIDE `data`
        // next to `id` — a nesting that is easy to get wrong by hand and
        // invisible in a round-trip test, which only proves this crate agrees
        // with itself.
        events.extend([
            BusEvent::Channel {
                id: 3,
                message: ChildMessage::Ready,
            },
            BusEvent::Channel {
                id: 3,
                message: ChildMessage::Metric {
                    name: "rps".to_string(),
                    value: 42.0,
                },
            },
            BusEvent::Channel {
                id: 3,
                message: ChildMessage::ActionReply {
                    action: "gc".to_string(),
                    body: "freed 12MB".to_string(),
                    id: Some(7),
                },
            },
        ]);

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

    /// fails if a shepherd-channel message maps to the wrong dotted topic. A
    /// subscriber that asked for `channel.metric` and silently receives nothing
    /// has no other way to find out, and `channel.*` matches whatever typo is
    /// there — so the exact strings are the contract, not the prefix.
    #[test]
    fn every_shepherd_channel_message_has_its_own_topic() {
        for (message, topic) in [
            (ChildMessage::Ready, "channel.ready"),
            (
                ChildMessage::Metric {
                    name: "rps".to_string(),
                    value: 42.0,
                },
                "channel.metric",
            ),
            (
                ChildMessage::ActionReply {
                    action: "gc".to_string(),
                    body: "ok".to_string(),
                    id: Some(7),
                },
                "channel.action_reply",
            ),
        ] {
            let event = BusEvent::Channel {
                id: 3,
                message: message.clone(),
            };
            assert_eq!(event.topic(), topic, "{message:?}");
        }
    }

    /// fails if `channel.*` stops reaching every one of the three. The glob a
    /// dashboard writes is the prefix, so a topic that drifted out from under it
    /// (`channel_ready`, say) would be unreachable by the only pattern anyone
    /// actually subscribes with.
    #[test]
    fn the_channel_glob_reaches_all_three_topics() {
        for message in [
            ChildMessage::Ready,
            ChildMessage::Metric {
                name: "rps".to_string(),
                value: 1.0,
            },
            ChildMessage::ActionReply {
                action: "gc".to_string(),
                body: String::new(),
                id: None,
            },
        ] {
            let topic = BusEvent::Channel { id: 1, message }.topic();
            assert!(
                topic.starts_with("channel."),
                "`{topic}` is not under the channel.* glob"
            );
        }
    }

    /// fails if the event stops carrying the message body. The whole argument for
    /// putting the real message on the bus rather than a summary is that nothing
    /// on this wire is a credential — a reply body that arrived truncated or
    /// replaced would make the topic useless for the case it exists for, a
    /// dashboard watching what apps actually say.
    #[test]
    fn a_channel_event_carries_the_message_verbatim() {
        let event = BusEvent::Channel {
            id: 3,
            message: ChildMessage::ActionReply {
                action: "gc".to_string(),
                body: "freed 12MB".to_string(),
                id: Some(7),
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("freed 12MB"), "{json}");
        assert_eq!(serde_json::from_str::<BusEvent>(&json).unwrap(), event);
    }
}
