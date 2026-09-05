//! `signal`: send a unix signal to matched sheep's own processes, never
//! their process group.
//!
//! The signal name is validated locally so a malformed one costs no round
//! trip. The daemon re-validates it anyway: peer input is untrusted.
//!
//! A row's own outcome is never a request failure. `shep signal` exits
//! non-`Success` only when the RPC itself failed.

use shep_client::Client;
use shep_core::protocol::{Request, Response, SelectorSpec};
use shep_core::signals::OperatorSignal;

use crate::cli::SignalArgs;
use crate::commands::selector::parse_selector;
use crate::exit::ExitCode;
use crate::output::{SignalledRows, Streams, emit, write_outcome};

/// Sends `args.signal` to the sheep matching `args.selector`, and renders one
/// row per match.
pub async fn signal(client: &Client, streams: &mut Streams<'_>, args: &SignalArgs) -> ExitCode {
    let selector = match parse_selector(streams, &args.selector) {
        Ok(selector) => SelectorSpec::from(&selector),
        Err(code) => return code,
    };

    let Some(sig) = OperatorSignal::parse(&args.signal) else {
        let message = format!(
            "`{}` is not a signal shep will send; accepted: {}",
            args.signal,
            OperatorSignal::ACCEPTED.join(", ")
        );
        return streams.fail(ExitCode::Usage, &message);
    };

    let body = Request::Signal {
        selector,
        // Canonical spelling: the wire carries `SIGHUP` whether the operator
        // typed `hup`, `Hup` or `SIGHUP`.
        signal: sig.as_str().to_string(),
    };

    match client.request(body).await {
        Ok(Response::Signalled(replies)) => write_outcome(emit(
            &mut *streams.out,
            streams.fmt,
            "signal",
            SignalledRows(replies),
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

    fn args(selector: &str, signal: &str) -> SignalArgs {
        SignalArgs {
            selector: selector.to_string(),
            signal: signal.to_string(),
        }
    }

    /// `"/[/"` is one of the only three inputs the selector grammar rejects.
    #[tokio::test]
    async fn a_malformed_selector_is_a_local_usage_error() {
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
            signal(&client, &mut streams, &args("/[/", "hup")).await
        };
        assert_eq!(code, ExitCode::Usage);
        assert!(
            envelopes.try_recv().is_err(),
            "a malformed selector must fail locally, never reach the wire"
        );
    }

    #[tokio::test]
    async fn a_bad_signal_name_never_reaches_the_wire() {
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
            signal(&client, &mut streams, &args("web", "SIGHUPP")).await
        };
        assert_eq!(code, ExitCode::Usage);
        assert!(
            envelopes.try_recv().is_err(),
            "a malformed signal name must fail locally, never reach the wire"
        );
        assert!(out.is_empty());
        assert!(!err.is_empty());
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
            signal(&client, &mut streams, &args("nonexistent", "hup")).await
        };
        assert_eq!(code, ExitCode::NotFound);
        assert!(out.is_empty());
    }

    /// Lowercase input on purpose: the canonical-spelling assertion is
    /// meaningless against an already-canonical name.
    #[tokio::test]
    async fn the_request_carries_the_selector_and_the_canonical_signal_name() {
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
        let _ = signal(&client, &mut streams, &args("web", "hup")).await;

        let envelope = envelopes.recv().await.unwrap();
        assert_eq!(
            envelope.body,
            Request::Signal {
                selector: SelectorSpec::Name("web".to_string()),
                signal: "SIGHUP".to_string(),
            }
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
            signal(&client, &mut streams, &args("web", "hup")).await
        };
        assert_eq!(code, ExitCode::Internal);
        assert!(out.is_empty());
        assert!(!err.is_empty());
    }
}
