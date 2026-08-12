//! `trigger`: send a named action to matched sheep over the shepherd channel
//! and report what each app answered.
//!
//! One verb, one call site, so unlike `lifecycle`/`logs`/`query` this module
//! carries no shared `request_and_render` helper of its own — inlining the
//! one match `trigger` needs reads more plainly than a generic wrapper
//! nothing else here would call.
//!
//! Sent with [`TRIGGER_DEADLINE`] rather than the client's 5s default: the
//! daemon waits on every matched sheep's own `action_timeout`
//! (`AppConfig::action_timeout`), which can be configured up to 58s, and the
//! default budget would abandon a reply the daemon is still honestly
//! building. See that constant's own doc for the exact margin.
//!
//! A row's own outcome — `Replied`, `NoChannel`, `Skipped`, `TimedOut` — is
//! never a request failure: the daemon reports it per matched sheep,
//! following `Reopen`/`Flush`'s own precedent, so `shep trigger` exits
//! non-`Success` only when the RPC itself failed (a malformed selector
//! caught locally, a selector that matched nothing, a daemon this client
//! could not reach). What each row says is `TriggeredRows`'s job
//! (`output/rows.rs`), not this module's.

use shep_client::{Client, TRIGGER_DEADLINE};
use shep_core::protocol::{Request, Response, SelectorSpec};

use crate::cli::{Format, TriggerArgs};
use crate::commands::selector::parse_selector;
use crate::exit::ExitCode;
use crate::output::{Streams, TriggeredRows, emit, emit_error, write_outcome};

/// Sends `args.action` (and `args.params`, if any) to the sheep matching
/// `args.selector`, and renders one row per match.
///
/// `action`/`params` are carried to the daemon exactly as typed — free-form
/// and unvalidated on this side, matching the wire's own
/// `Request::Trigger`, which the daemon never parses either. An app that
/// does not recognize the action name is expected to say so in its own
/// reply.
pub async fn trigger(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    args: &TriggerArgs,
) -> ExitCode {
    let selector = match parse_selector(streams, fmt, &args.selector) {
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
            fmt,
            "trigger",
            TriggeredRows(replies),
        )),
        Ok(_unrecognised) => {
            let message = "the daemon answered with a response this client does not understand";
            let _ = emit_error(
                &mut *streams.err,
                fmt,
                ExitCode::Internal.code_str(),
                message,
            );
            ExitCode::Internal
        }
        Err(err) => {
            let code = ExitCode::from(&err);
            let _ = emit_error(&mut *streams.err, fmt, code.code_str(), &err.to_string());
            code
        }
    }
}

#[cfg(test)]
mod tests {
    use shep_client::testing::{fake_client_capturing_envelopes, fake_client_replying_err};
    use shep_core::protocol::RpcErrorCode;

    use super::*;

    fn args(selector: &str, action: &str, params: Option<&str>) -> TriggerArgs {
        TriggerArgs {
            selector: selector.to_string(),
            action: action.to_string(),
            params: params.map(str::to_string),
        }
    }

    /// `"/[/"` is one of the only three inputs the selector grammar rejects.
    /// A verb that skipped the client-side parse would send it and exit
    /// `NotFound` instead of `Usage`, and the daemon would see a request it
    /// never should have.
    #[tokio::test]
    async fn a_malformed_selector_exits_usage_without_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            trigger(
                &client,
                &mut streams,
                Format::Table,
                &args("/[/", "ping", None),
            )
            .await
        };
        assert_eq!(code, ExitCode::Usage);
        assert!(
            envelopes.try_recv().is_err(),
            "a malformed selector must fail locally, never reach the wire"
        );
    }

    /// A selector that matched no registered sheep is the one way `trigger`
    /// fails as a whole request (`shep-daemon`'s own `trigger` returns
    /// `SupervisorError::NotFound` from `selector_of` exactly as every other
    /// selector-taking verb does) — distinct from a matched sheep with no
    /// channel, which is a `NoChannel` *row*, not a failure.
    #[tokio::test]
    async fn a_not_found_reply_exits_not_found_rather_than_being_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, _served) =
            fake_client_replying_err(&path, RpcErrorCode::NotFound, "no sheep matched").await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            trigger(
                &client,
                &mut streams,
                Format::Table,
                &args("nonexistent", "ping", None),
            )
            .await
        };
        assert_eq!(code, ExitCode::NotFound);
        assert!(out.is_empty());
    }

    /// The envelope's own `body`, not just that the call succeeded: a
    /// selector converted with the wrong helper, an action silently dropped,
    /// or `params` mixed up with `action` all still get a reply from the
    /// fake daemon (it always answers `Pong`) and only this catches them.
    /// The deadline is asserted alongside for the same reason
    /// `lifecycle::start_asks_for_the_longer_deadline` checks its own: a
    /// `trigger` sent with the client's plain 5s default would abandon a
    /// reply the daemon is still honestly building for a sheep with a long
    /// `action_timeout`.
    #[tokio::test]
    async fn the_request_carries_the_selector_action_params_and_trigger_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };
        let _ = trigger(
            &client,
            &mut streams,
            Format::Table,
            &args("web", "gc", Some("--force")),
        )
        .await;

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

    /// A response this client does not recognise (the fake daemon's generic
    /// `Pong`, standing in for a `Response` variant `trigger`'s `match` has
    /// no arm for) must not be read as any of the four outcomes — it is
    /// `Internal`, the same rule every other verb's `extract` follows for
    /// `Response`'s `#[non_exhaustive]`.
    #[tokio::test]
    async fn an_unrecognised_response_exits_internal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, _envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            trigger(
                &client,
                &mut streams,
                Format::Table,
                &args("web", "ping", None),
            )
            .await
        };
        assert_eq!(code, ExitCode::Internal);
        assert!(out.is_empty());
        assert!(!err.is_empty());
    }
}
