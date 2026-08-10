//! The shepherd channel: fd-3 socketpair carrying newline-delimited JSON
//! between the daemon and each spawned child.
//!
//! [`ChildMessage`] flows child -> daemon (readiness, metrics, action
//! replies); [`ShepherdMessage`] flows daemon -> child (shutdown request,
//! custom actions). Framing (newline-JSON over `BufReader::lines()`) is
//! wired by the real runner; this module only pins the message shapes.
//!
//! Public for two reasons, neither of them a shipped API: `tests/real_runner.rs`
//! reads [`ChildMessage`] back off a real child's fd 3, and [`ShepherdMessage`]
//! is the payload type of [`ProcIo::to_child`](crate::runner::ProcIo::to_child),
//! so it has to be nameable wherever that field is.

use serde::{Deserialize, Serialize};

/// Child→daemon shepherd-channel message (spec §7 — kebab-case kinds)
// wire format: changing these strings is a breaking change
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ChildMessage {
    /// `{"kind":"ready"}` — readiness signal (`wait_ready` gate)
    Ready,
    /// Custom metric sample
    Metric {
        /// Metric name
        name: String,
        /// Metric value
        value: f64,
    },
    /// Reply to a daemon-initiated action
    ActionReply {
        /// The action name this replies to
        action: String,
        /// Free-form reply body
        body: String,
    },
}

/// Daemon→child message
// wire format: changing these strings is a breaking change
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ShepherdMessage {
    /// Graceful-stop request (`shutdown_with_message`)
    Shutdown,
    /// Custom action dispatch
    Action {
        /// The action name
        name: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixtures pinned FROM SPEC STRINGS (spec §7) — round-tripped both ways so a
    // silent field/rename drift fails loudly in either direction.

    #[test]
    fn ready_wire_fixture_round_trips() {
        let fixture = r#"{"kind":"ready"}"#;
        assert_eq!(
            serde_json::from_str::<ChildMessage>(fixture).unwrap(),
            ChildMessage::Ready
        );
        assert_eq!(
            serde_json::to_string(&ChildMessage::Ready).unwrap(),
            fixture
        );
    }

    #[test]
    fn metric_wire_fixture_round_trips() {
        let fixture = r#"{"kind":"metric","name":"rps","value":42.0}"#;
        let msg = ChildMessage::Metric {
            name: "rps".to_string(),
            value: 42.0,
        };
        assert_eq!(serde_json::from_str::<ChildMessage>(fixture).unwrap(), msg);
        assert_eq!(serde_json::to_string(&msg).unwrap(), fixture);
    }

    #[test]
    fn action_reply_wire_fixture_round_trips() {
        let fixture = r#"{"kind":"action-reply","action":"x","body":"y"}"#;
        let msg = ChildMessage::ActionReply {
            action: "x".to_string(),
            body: "y".to_string(),
        };
        assert_eq!(serde_json::from_str::<ChildMessage>(fixture).unwrap(), msg);
        assert_eq!(serde_json::to_string(&msg).unwrap(), fixture);
    }

    #[test]
    fn shutdown_wire_fixture_round_trips() {
        let fixture = r#"{"kind":"shutdown"}"#;
        assert_eq!(
            serde_json::from_str::<ShepherdMessage>(fixture).unwrap(),
            ShepherdMessage::Shutdown
        );
        assert_eq!(
            serde_json::to_string(&ShepherdMessage::Shutdown).unwrap(),
            fixture
        );
    }

    #[test]
    fn action_wire_fixture_round_trips() {
        let fixture = r#"{"kind":"action","name":"gc"}"#;
        let msg = ShepherdMessage::Action {
            name: "gc".to_string(),
        };
        assert_eq!(
            serde_json::from_str::<ShepherdMessage>(fixture).unwrap(),
            msg
        );
        assert_eq!(serde_json::to_string(&msg).unwrap(), fixture);
    }
}
