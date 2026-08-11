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
        /// Argument text for the action, passed through to the child
        /// verbatim; `None` when the action was triggered without any.
        ///
        /// Absent from the serialized form when `None`, and absent on the
        /// wire deserializes back to `None`, so a message carrying no
        /// arguments is byte-identical to one from before this field
        /// existed. That is what makes the field additive on a channel that
        /// has no version to negotiate — see the spec's §9 note on
        /// `trigger`.
        ///
        /// One opaque string, not structured data: the daemon never reads
        /// it, and an app that wants JSON, a flag list or a bare word parses
        /// it in the grammar it already has.
        // `skip_serializing_if` is the load-bearing half: without it a
        // message with no arguments goes out as `"params":null` instead of
        // no key at all. `default` is redundant today — serde's derive
        // already reads a missing `Option` field back as `None`, and no test
        // can tell whether it is here — and is written anyway because that
        // is a property of the derive rather than of this field, and a
        // change of type would withdraw it silently on a channel that has no
        // version in which to announce one.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        params: Option<String>,
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
            params: None,
        };
        assert_eq!(
            serde_json::from_str::<ShepherdMessage>(fixture).unwrap(),
            msg
        );
        assert_eq!(serde_json::to_string(&msg).unwrap(), fixture);
    }

    /// The with-arguments form of the fixture above, pinned the same way so
    /// that renaming or reordering either field fails here rather than at an
    /// app that stopped understanding its own actions.
    ///
    /// It does not overlap with the fixture above, and each catches what the
    /// other cannot. That one is what an argument-free action must keep
    /// looking like: its serialize half is what fails if
    /// `skip_serializing_if` is dropped — measured, and the message is
    /// `"params":null` against `{"kind":"action","name":"gc"}` — and its
    /// deserialize half is the proof that a message written before the field
    /// existed still reads. This one is the only thing pinning the field's
    /// name and its position when there is an argument: rename `params`, or
    /// declare it ahead of `name`, and this test fails while the whole rest
    /// of the workspace goes on passing.
    #[test]
    fn action_with_params_wire_fixture_round_trips() {
        let fixture = r#"{"kind":"action","name":"set-log-level","params":"debug"}"#;
        let msg = ShepherdMessage::Action {
            name: "set-log-level".to_string(),
            params: Some("debug".to_string()),
        };
        assert_eq!(
            serde_json::from_str::<ShepherdMessage>(fixture).unwrap(),
            msg
        );
        assert_eq!(serde_json::to_string(&msg).unwrap(), fixture);
    }
}
