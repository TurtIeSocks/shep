//! `trigger`: send a named action to matched sheep over the shepherd channel
//! and report what each app answered.
//!
//! Sent with [`TRIGGER_DEADLINE`], not the client's 5s default: the daemon
//! waits on every matched sheep's own `AppConfig::action_timeout`, which can
//! be configured up to 58s.
//!
//! A row's own outcome is never a request failure. `shep trigger` exits
//! non-`Success` only when the RPC itself failed.

use shep_client::{Client, TRIGGER_DEADLINE};
use shep_core::protocol::{Request, Response, SelectorSpec};

use crate::cli::TriggerArgs;
use crate::commands::selector::parse_selector;
use crate::exit::ExitCode;
use crate::output::{Streams, TriggeredRows, emit, write_outcome};

/// Sends `args.action` (and `args.params`, if any) to the sheep matching
/// `args.selector`, and renders one row per match.
///
/// `action` and `params` are carried exactly as typed: neither this side nor
/// the daemon parses them.
pub async fn trigger(client: &Client, streams: &mut Streams<'_>, args: &TriggerArgs) -> ExitCode {
    let selector = match parse_selector(streams, &args.selector) {
        Ok(selector) => SelectorSpec::from(&selector),
        Err(code) => return code,
    };

    let body = Request::Trigger {
        selector,
        action: args.action.clone(),
        params: args.params.clone(),
    };

    match client
        .request_with_deadline(body, Some(TRIGGER_DEADLINE))
        .await
    {
        Ok(Response::Triggered(replies)) => write_outcome(emit(
            &mut *streams.out,
            streams.fmt,
            "trigger",
            TriggeredRows(replies),
            streams.style,
        )),
        Ok(_unrecognised) => {
            let message = "the daemon answered with a response this client does not understand";
            streams.fail(ExitCode::Internal, message)
        }
        Err(err) => {
            let code = ExitCode::from(&err);
            streams.fail(code, &err.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use shep_client::testing::{fake_client_capturing_envelopes, fake_client_replying_err};
    use shep_core::protocol::RpcErrorCode;

    use super::*;
    use crate::cli::Format;

    fn args(selector: &str, action: &str, params: Option<&str>) -> TriggerArgs {
        TriggerArgs {
            selector: selector.to_string(),
            action: action.to_string(),
            params: params.map(str::to_string),
        }
    }

    /// `"/[/"` is one of the only three inputs the selector grammar rejects.
    #[tokio::test]
    async fn a_malformed_selector_exits_usage_without_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            trigger(&client, &mut streams, &args("/[/", "ping", None)).await
        };
        assert_eq!(code, ExitCode::Usage);
        assert!(
            envelopes.try_recv().is_err(),
            "a malformed selector must fail locally, never reach the wire"
        );
    }

    #[tokio::test]
    async fn a_not_found_reply_exits_not_found_rather_than_being_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, _served) =
            fake_client_replying_err(&path, RpcErrorCode::NotFound, "no sheep matched").await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            trigger(&client, &mut streams, &args("nonexistent", "ping", None)).await
        };
        assert_eq!(code, ExitCode::NotFound);
        assert!(out.is_empty());
    }

    /// The fake daemon always answers `Pong`; only the captured envelope
    /// catches a dropped field.
    #[tokio::test]
    async fn the_request_carries_the_selector_action_params_and_trigger_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
            style: crate::style::Presentation::BARE,
            fmt: Format::Table,
        };
        let _ = trigger(&client, &mut streams, &args("web", "gc", Some("--force"))).await;

        let envelope = envelopes.recv().await.unwrap();
        assert_eq!(
            envelope.body,
            Request::Trigger {
                selector: SelectorSpec::Name("web".to_string()),
                action: "gc".to_string(),
                params: Some("--force".to_string()),
            }
        );
        assert_eq!(
            envelope.deadline_ms,
            Some(u64::try_from(TRIGGER_DEADLINE.as_millis()).unwrap()),
            "a trigger sent with the client's plain default would abandon a reply the daemon \
             is still honestly building"
        );
    }

    /// The fake daemon's `Pong` stands in for a `Response` variant this
    /// `match` has no arm for.
    #[tokio::test]
    async fn an_unrecognised_response_exits_internal() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, _envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            trigger(&client, &mut streams, &args("web", "ping", None)).await
        };
        assert_eq!(code, ExitCode::Internal);
        assert!(out.is_empty());
        assert!(!err.is_empty());
    }
}
