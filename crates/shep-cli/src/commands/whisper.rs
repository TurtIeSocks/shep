//! `whisper`: write one line to matched sheep's stdin.
//!
//! A line carrying an embedded newline or carriage return is refused
//! locally, so it costs no round trip. The daemon re-validates it anyway:
//! peer input is untrusted.
//!
//! A row's own outcome is never a request failure. `shep whisper` exits
//! non-`Success` only when the RPC itself failed.
//!
//! `sent` means the bytes were written and flushed to the pipe, not that the
//! app read them.

use shep_client::Client;
use shep_core::protocol::{Request, Response, SelectorSpec};

use crate::cli::WhisperArgs;
use crate::commands::selector::parse_selector;
use crate::exit::ExitCode;
use crate::output::{SentLineRows, Streams, emit, write_outcome};

/// Writes `args.line` to the stdin of the sheep matching `args.selector`,
/// and renders one row per match.
pub async fn whisper(client: &Client, streams: &mut Streams<'_>, args: &WhisperArgs) -> ExitCode {
    let selector = match parse_selector(streams, &args.selector) {
        Ok(selector) => SelectorSpec::from(&selector),
        Err(code) => return code,
    };

    if args.line.contains('\n') || args.line.contains('\r') {
        let message = "the line must be one line — an embedded newline or carriage return \
                        would deliver two commands where you typed one; shep adds the \
                        terminator itself";
        return streams.fail(ExitCode::Usage, message);
    }

    let body = Request::SendLine {
        selector,
        line: args.line.clone(),
    };

    match client.request(body).await {
        Ok(Response::SentLine(rows)) => write_outcome(emit(
            &mut *streams.out,
            streams.fmt,
            "whisper",
            SentLineRows(rows),
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

    fn args(selector: &str, line: &str) -> WhisperArgs {
        WhisperArgs {
            selector: selector.to_string(),
            line: line.to_string(),
        }
    }

    /// Runs the verb against a fake daemon that captures envelopes.
    async fn run(
        args: &WhisperArgs,
    ) -> (
        ExitCode,
        Vec<u8>,
        Vec<u8>,
        tokio::sync::mpsc::Receiver<shep_core::protocol::Envelope>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            whisper(&client, &mut streams, args).await
        };
        (code, out, err, envelopes)
    }

    /// `"/[/"` is one of the only three inputs the selector grammar rejects.
    #[tokio::test]
    async fn a_malformed_selector_exits_usage_without_a_round_trip() {
        let (code, _out, _err, mut envelopes) = run(&args("/[/", "gc")).await;
        assert_eq!(code, ExitCode::Usage);
        assert!(
            envelopes.try_recv().is_err(),
            "a malformed selector must fail locally, never reach the wire"
        );
    }

    #[tokio::test]
    async fn a_line_with_an_embedded_newline_exits_usage_without_a_round_trip() {
        let (code, _out, err, mut envelopes) = run(&args("repl", "gc\nquit")).await;
        assert_eq!(code, ExitCode::Usage);
        assert!(
            envelopes.try_recv().is_err(),
            "a line with a newline must fail locally, never reach the wire"
        );
        let rendered = String::from_utf8(err).unwrap();
        assert!(rendered.contains("one line"), "{rendered}");
    }

    /// `\r` is the one an operator produces by accident, pasting CRLF text.
    #[tokio::test]
    async fn a_line_with_a_carriage_return_is_refused_too() {
        let (code, _out, _err, mut envelopes) = run(&args("repl", "gc\r")).await;
        assert_eq!(code, ExitCode::Usage);
        assert!(envelopes.try_recv().is_err());
    }

    /// The wire's contract is that the line carries no terminator; the
    /// shepherd's writer is the single place that adds one.
    #[tokio::test]
    async fn the_request_carries_the_selector_and_the_bare_line() {
        let (_code, _out, _err, mut envelopes) = run(&args("repl", "gc")).await;
        let envelope = envelopes.recv().await.unwrap();
        assert_eq!(
            envelope.body,
            Request::SendLine {
                selector: SelectorSpec::Name("repl".to_string()),
                line: "gc".to_string(),
            }
        );
    }

    /// The fake daemon's `Pong` stands in for a `Response` variant this
    /// `match` has no arm for.
    #[tokio::test]
    async fn an_unrecognised_response_exits_internal() {
        let (code, out, err, _envelopes) = run(&args("repl", "gc")).await;
        assert_eq!(code, ExitCode::Internal);
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
            whisper(&client, &mut streams, &args("ghost", "gc")).await
        };
        assert_eq!(code, ExitCode::NotFound);
        assert!(out.is_empty());
    }
}
