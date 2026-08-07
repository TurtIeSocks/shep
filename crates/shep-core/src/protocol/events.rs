//! Bus events broadcast to subscribed clients

use serde::{Deserialize, Serialize};

use crate::protocol::request::ProcessInfo;

/// What happened to a sheep
// wire format: changing existing variants is a breaking change
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
    /// Stopped by request
    Stop,
    /// Deregistered
    Delete,
    /// Restart budget exhausted
    Errored,
}

/// One event on the daemon bus
///
/// The serde tag is structural; subscription TOPICS are the dotted
/// strings from [`BusEvent::topic`] (`process.exit`, `log.out`, `daemon.*` —
/// spec §6 grammar). Phase 2's server-side filter globs against `topic()`.
// wire format: changing existing variants is a breaking change
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
        let events = vec![
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
}
