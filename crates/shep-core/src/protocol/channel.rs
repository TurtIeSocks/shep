//! The shepherd channel: the newline-JSON wire carried on fd 3 between the
//! shepherd and each spawned child.
//!
//! [`ChildMessage`] flows child -> shepherd (readiness, metrics, action
//! replies); [`ShepherdMessage`] flows shepherd -> child (shutdown request,
//! custom actions). Framing (newline-JSON over `BufReader::lines()`) is wired
//! by shep-daemon's real runner; this module only pins the message shapes.
//!
//! # Why this lives in shep-core
//!
//! It did not, until `BusEvent::Channel` (spec §6's `channel.*` topic) began
//! carrying a [`ChildMessage`] verbatim to every subscriber. A bus event is a
//! shep-core type, so the message it carries has to be one too — and a second
//! copy of these shapes in shep-daemon would be two spellings of one wire that
//! no test could compare across the crate boundary. shep-daemon re-exports
//! both types from its own `channel` module, so nothing that already names
//! them had to change.
//!
//! Both enums are deliberately NOT `#[non_exhaustive]`, unlike everything else
//! under `protocol`. There is no handshake on fd 3 and no version to negotiate
//! (`CHANNEL_VERSION` is a stamp, not a negotiation — see its own doc), so a
//! new variant here is a change every app that speaks this wire has to be told
//! about out of band. Leaving them exhaustive means the compiler names every
//! site that has to decide something, [`BusEvent::topic`] included, which is
//! exactly the review a change on this wire deserves.
//!
//! This module pins the wire shapes; it is not the app-author-facing contract.
//! An app that wants to speak this wire — including why it should reply to a
//! [`ShepherdMessage::Action`] even when it does not recognize the name, how an
//! echoed `id` gets a reply matched to its exact trigger and what the
//! name-and-order fallback costs an app that does not echo it, and the `params`
//! quoting gap — wants `docs/shepherd-channel.md` at the repository root.

use serde::{Deserialize, Serialize};

/// The value the shepherd exports as `SHEP_CHANNEL_VERSION` to every child it
/// opens a channel for.
///
/// One version, and it stays `"1"` through this field addition, because the
/// addition is additive in both directions: a daemon that stamps and an app
/// that ignores the stamp interoperate exactly as before. What the variable
/// buys is not negotiation — the shepherd still cannot ask an app what it
/// speaks — but the ability for a defensive app to notice that fd 3 is
/// carrying a protocol it has never seen, instead of failing to parse a line
/// with nothing anywhere connecting that failure to a protocol change.
///
/// `docs/shepherd-channel.md` is the definition of what `"1"` means.
pub const CHANNEL_VERSION: &str = "1";

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
        /// The `id` of the [`ShepherdMessage::Action`] this answers, echoed
        /// back verbatim. `None` when the app did not echo it.
        ///
        /// Optional, and that is the whole design. An app that echoes gets
        /// exact correlation: its reply reaches the wait that asked, even
        /// when an earlier trigger of the same action name timed out and is
        /// still owed a reply. An app that does not echo — every app written
        /// before this field existed — sends no `id` key at all, and the
        /// daemon falls back to matching by name and order exactly as it did
        /// before. Nothing already speaking this channel breaks, which is
        /// what makes the field additive on a wire with no handshake.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        id: Option<u64>,
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
        /// This dispatch's correlation id, unique for the life of the
        /// daemon. Echo it back on your [`ChildMessage::ActionReply`] as
        /// `id` and the daemon matches your answer to this exact request
        /// rather than to its name.
        ///
        /// Always present, unlike `params`: an app that ignores the key is
        /// unaffected, and an app that wants to echo must never have to
        /// handle its absence. `u64` and monotonically increasing, but
        /// neither of those is a promise an app should lean on — treat it as
        /// an opaque token to hand back.
        id: u64,
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

    /// fails if a reply that carries no `id` stops deserializing — the
    /// spelling every app written before Phase 10 sends, and the one the
    /// name-and-order fallback exists for.
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

    /// fails if an echoed `id` is dropped on the way in, or emitted when
    /// absent on the way out. Both directions, because the daemon writes
    /// this type in tests and reads it in production.
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

    /// fails if the daemon stops writing `id` on an action, or starts
    /// writing `params` when there is none. `id` is unconditional and
    /// `params` is not — the two halves of the same line.
    ///
    /// Both directions per case, not just serialize: `id` being new is what
    /// changed here, but `params`'s own additive round-trip (the field this
    /// module already pinned before this task) still has to keep holding.
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
