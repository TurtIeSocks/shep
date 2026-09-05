//! The shepherd channel: newline-JSON wire on fd 3 between the shepherd and
//! each spawned child. [`ChildMessage`] flows child -> shepherd;
//! [`ShepherdMessage`] flows shepherd -> child. Framing is wired by
//! shep-daemon; lives in shep-core because `BusEvent::Channel` carries a
//! `ChildMessage` verbatim to every bus subscriber.
//!
//! Both enums are exhaustive, unlike everything else under `protocol`: fd 3
//! has no handshake, so a new variant means telling every app out of band,
//! and exhaustive matches force every call site to react to it.
//!
//! Pins the wire shapes only, not the app-facing contract: see
//! `docs/shepherd-channel.md` for reply and correlation semantics.

use serde::{Deserialize, Serialize};

/// The value the shepherd exports as `SHEP_CHANNEL_VERSION` to every child it
/// opens a channel for.
///
/// Stays `"1"` through this field addition: a daemon that stamps and an app
/// that ignores the stamp interoperate exactly as before. Not a
/// negotiation, just a way for a defensive app to notice that fd 3 carries
/// a protocol it has never seen.
///
/// `docs/shepherd-channel.md` defines what `"1"` means.
pub const CHANNEL_VERSION: &str = "1";

/// Child -> daemon shepherd-channel message (spec §7, kebab-case kinds)
// wire format: changing these strings is a breaking change
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ChildMessage {
    /// `{"kind":"ready"}`: readiness signal (`wait_ready` gate)
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
        /// The `id` of the [`ShepherdMessage::Action`] this answers, echoed
        /// back verbatim. `None` when the app did not echo it, in which
        /// case the daemon falls back to matching by name and order.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        id: Option<u64>,
    },
}

/// Daemon -> child message
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
        /// verbatim; `None` when triggered without any.
        ///
        /// Omitted from the wire when `None`, so a message with no
        /// arguments round-trips byte-identical.
        ///
        /// One opaque string, not structured data: the daemon never reads
        /// it, so an app parses it in whatever grammar it already has.
        // `skip_serializing_if` is load-bearing: without it a message with
        // no arguments serializes `"params":null` instead of omitting the
        // key. `default` guards a future type change on a channel with no
        // version to announce one.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        params: Option<String>,
        /// This dispatch's correlation id, unique for the life of the
        /// daemon. Echo it back on your [`ChildMessage::ActionReply`] as
        /// `id` and the daemon matches your answer to this exact request
        /// rather than to its name.
        ///
        /// Always present, unlike `params`. Treat it as an opaque token to
        /// hand back: `u64` and increasing are implementation details, not
        /// a promise.
        id: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixtures pinned from spec §7 strings, round-tripped both ways so a
    // silent field or rename drift fails loudly.

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

    /// Apps with no correlation id still send this shape.
    #[test]
    fn an_action_reply_without_an_id_round_trips() {
        let fixture = r#"{"kind":"action-reply","action":"gc","body":"ok"}"#;
        let msg = ChildMessage::ActionReply {
            action: "gc".to_string(),
            body: "ok".to_string(),
            id: None,
        };
        assert_eq!(serde_json::from_str::<ChildMessage>(fixture).unwrap(), msg);
        assert_eq!(serde_json::to_string(&msg).unwrap(), fixture);
    }

    #[test]
    fn an_action_reply_with_an_echoed_id_round_trips() {
        let fixture = r#"{"kind":"action-reply","action":"gc","body":"ok","id":7}"#;
        let msg = ChildMessage::ActionReply {
            action: "gc".to_string(),
            body: "ok".to_string(),
            id: Some(7),
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

    /// `id` is unconditional; `params` is not. Both cases, both directions.
    #[test]
    fn an_action_carries_its_id_with_or_without_params() {
        let bare = r#"{"kind":"action","name":"gc","id":7}"#;
        let bare_msg = ShepherdMessage::Action {
            name: "gc".to_string(),
            params: None,
            id: 7,
        };
        assert_eq!(serde_json::to_string(&bare_msg).unwrap(), bare);
        assert_eq!(
            serde_json::from_str::<ShepherdMessage>(bare).unwrap(),
            bare_msg
        );

        let with_params = r#"{"kind":"action","name":"set-log-level","params":"debug","id":8}"#;
        let with_params_msg = ShepherdMessage::Action {
            name: "set-log-level".to_string(),
            params: Some("debug".to_string()),
            id: 8,
        };
        assert_eq!(
            serde_json::to_string(&with_params_msg).unwrap(),
            with_params
        );
        assert_eq!(
            serde_json::from_str::<ShepherdMessage>(with_params).unwrap(),
            with_params_msg
        );
    }
}
