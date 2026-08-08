//! Anything the daemon writes to a connected client
//!
//! The server sends two kinds of frames on one socket: [`Reply`] (answers to
//! requests) and [`BusEvent`] (broadcast events). This type decodes either,
//! untagged, because their JSON key sets are disjoint (`id`/`result` vs `event`).
//! Untagged costs **zero wire bytes** and keeps fixtures backward-compatible.
//!
//! Deserialization needs `serde/std` for buffered content semantics; this is
//! available via `serde_json`'s `std` feature (already enabled workspace-wide).

use serde::{Deserialize, Serialize};

use crate::protocol::{BusEvent, Reply};

/// Anything the daemon writes to a connected client
///
/// Untagged on purpose: `Reply` and `BusEvent` have disjoint key sets, so
/// this decodes either without adding a byte to the wire. The daemon
/// serializes `Reply`/`BusEvent` directly; a `ServerFrame` round-trips to
/// byte-identical output (pinned by `server_frame_is_byte_identical`).
// wire format: changing existing variants is a breaking change
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
// Growth is anticipated: a future frame kind (progress, flow control) is
// additive here and stays additive on the wire (IR-20).
#[non_exhaustive]
pub enum ServerFrame {
    /// An answer to one request (from [`Envelope`](crate::protocol::Envelope))
    Reply(Reply),
    /// One subscribed bus event
    Event(BusEvent),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        BusEvent, ProcessEventKind, ProcessInfo, Reply, Response, RpcError, RpcErrorCode,
        encode_frame,
    };
    use crate::status::ProcStatus;

    fn sample_reply() -> Reply {
        Reply {
            id: 7,
            result: Ok(Response::Pong),
        }
    }

    fn sample_event() -> BusEvent {
        BusEvent::Process {
            event: ProcessEventKind::Online,
            info: ProcessInfo {
                id: 3,
                name: "web".to_string(),
                status: ProcStatus::Online,
                pid: Some(4242),
                restarts: 0,
                uptime_ms: 0,
                fold: None,
                out_file: Some("/home/rin/.shep/logs/web-0-out.log".to_string()),
                err_file: Some("/home/rin/.shep/logs/web-0-err.log".to_string()),
            },
            manually: false,
            at_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn server_frame_decodes_both_directions_of_the_stream() {
        // The two shapes are disjoint: a Reply has no `event` key and an
        // event has no `id`/`result` pair, so untagged never guesses wrong.
        let reply = r#"{"id":1,"result":{"Ok":{"kind":"pong"}}}"#;
        assert!(matches!(
            serde_json::from_str::<ServerFrame>(reply).unwrap(),
            ServerFrame::Reply(Reply { id: 1, .. })
        ));
        let event = r#"{"event":"log_out","data":{"id":3,"line":"ready"}}"#;
        assert!(matches!(
            serde_json::from_str::<ServerFrame>(event).unwrap(),
            ServerFrame::Event(BusEvent::LogOut { id: 3, .. })
        ));
    }

    #[test]
    fn server_frame_is_byte_identical_to_its_payload() {
        // The daemon encodes Reply/BusEvent directly; if wrapping ever
        // started adding bytes, every client would break at once.
        let reply = sample_reply();
        assert_eq!(
            encode_frame(&ServerFrame::Reply(reply.clone())).unwrap(),
            encode_frame(&reply).unwrap()
        );
        let event = sample_event();
        assert_eq!(
            encode_frame(&ServerFrame::Event(event.clone())).unwrap(),
            encode_frame(&event).unwrap()
        );
    }

    #[test]
    fn an_error_reply_still_decodes_as_a_reply_frame() {
        let err = Reply {
            id: 2,
            result: Err(RpcError {
                code: RpcErrorCode::DeadlineExceeded,
                message: "request deadline of 5000 ms expired".to_string(),
            }),
        };
        let json = serde_json::to_string(&err).unwrap();
        assert_eq!(
            serde_json::from_str::<ServerFrame>(&json).unwrap(),
            ServerFrame::Reply(err)
        );
    }
}
