//! `whisper`: write one line to matched sheep's stdin.
//!
//! One verb, one call site, following `trigger`/`signal`'s own precedent:
//! inlining the one match this needs reads more plainly than a shared
//! generic wrapper nothing else here would call.
//!
//! A line carrying an embedded newline or carriage return is checked
//! locally, before the round trip — two commands where the operator typed
//! one is a usage error they caused, and it should cost neither a
//! connection nor a daemon round trip. The daemon re-validates anyway
//! (`shep-daemon`'s own `Request::SendLine` arm), because peer input is
//! untrusted and a client is not the only thing that can send a frame.
//!
//! Sent with the client's plain default deadline: nothing on this path
//! waits on the app the way `trigger` waits on its `action_timeout` — the
//! shepherd's own write is bounded well inside that default.
//!
//! A row's own outcome — `Sent`, `NoStdin`, `NotWritten` — is never a
//! request failure: the daemon reports it per matched sheep, following
//! `Trigger`/`Signal`'s own precedent, so `shep whisper` exits
//! non-`Success` only when the RPC itself failed (a malformed selector or
//! line caught locally, a selector that matched nothing, a daemon this
//! client could not reach). What each row says is `SentLineRows`'s job
//! (`output/rows.rs`), not this module's.
//!
//! `sent` means the bytes were written and flushed to the pipe — not that
//! the app read them. A pipe holds 64 KiB before it blocks, and there is
//! nothing on this path that could tell a read app from an app that never
//! reads its stdin at all.

use shep_client::Client;
use shep_core::protocol::{Request, Response, SelectorSpec};

use crate::cli::{Format, WhisperArgs};
use crate::commands::selector::parse_selector;
use crate::exit::ExitCode;
use crate::output::{SentLineRows, Streams, emit, emit_error, write_outcome};

/// Writes `args.line` to the stdin of the sheep matching `args.selector`,
/// and renders one row per match.
pub async fn whisper(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    args: &WhisperArgs,
) -> ExitCode {
    let selector = match parse_selector(streams, fmt, &args.selector) {
        Ok(selector) => SelectorSpec::from(&selector),
        Err(code) => return code,
    };

    if args.line.contains('\n') || args.line.contains('\r') {
        let message = "the line must be one line — an embedded newline or carriage return \
                        would deliver two commands where you typed one; shep adds the \
                        terminator itself";
        let _ = emit_error(&mut *streams.err, fmt, ExitCode::Usage.code_str(), message);
        return ExitCode::Usage;
    }

    let body = Request::SendLine {
        selector,
        line: args.line.clone(),
    };

    match client.request(body).await {
        Ok(Response::SentLine(rows)) => write_outcome(emit(
            &mut *streams.out,
            fmt,
            "whisper",
            SentLineRows(rows),
            streams.style,
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

    fn args(selector: &str, line: &str) -> WhisperArgs {
        WhisperArgs {
            selector: selector.to_string(),
            line: line.to_string(),
        }
    }

    /// Runs the verb against a fake daemon that captures envelopes, and hands
    /// back the exit code, stdout, stderr and the capture channel.
    async fn run(
        args: &WhisperArgs,
    ) -> (
        ExitCode,
        Vec<u8>,
        Vec<u8>,
        tokio::sync::mpsc::Receiver<shep_core::protocol::Envelope>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
            };
            whisper(&client, &mut streams, Format::Table, args).await
        };
        (code, out, err, envelopes)
    }

    /// `"/[/"` is one of the only three inputs the selector grammar rejects.
    /// A verb that skipped the client-side parse would send it and exit
    /// `NotFound` instead of `Usage`, and the daemon would see a request it
    /// never should have.
    #[tokio::test]
    async fn a_malformed_selector_exits_usage_without_a_round_trip() {
        let (code, _out, _err, mut envelopes) = run(&args("/[/", "gc")).await;
        assert_eq!(code, ExitCode::Usage);
        assert!(
            envelopes.try_recv().is_err(),
            "a malformed selector must fail locally, never reach the wire"
        );
    }

    /// fails if a line carrying a newline reaches the wire. The daemon refuses
    /// it too, and deliberately in both places — but the operator gets a faster
    /// and more specific answer from the side that knows what they typed, and
    /// this side must not spend a connection to learn it.
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

    /// fails if a carriage return slips through. `\r` is the one an operator
    /// produces by accident — pasting from a file with CRLF endings — and it
    /// reaches a shell as a command with a stray control character in it.
    #[tokio::test]
    async fn a_line_with_a_carriage_return_is_refused_too() {
        let (code, _out, _err, mut envelopes) = run(&args("repl", "gc\r")).await;
        assert_eq!(code, ExitCode::Usage);
        assert!(envelopes.try_recv().is_err());
    }

    /// The envelope's own `body`, not just that the call succeeded. Two
    /// mistakes only this catches: a selector converted with the wrong helper,
    /// and a terminator appended client-side — the wire's contract is that the
    /// line does NOT carry one, and the shepherd's writer is the single place
    /// that adds it.
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

    /// A response this client does not recognise (the fake daemon's generic
    /// `Pong`, standing in for a `Response` variant this verb's `match` has no
    /// arm for) must not be read as any of the outcomes — it is `Internal`, the
    /// rule every other verb's extract follows for `Response`'s
    /// `#[non_exhaustive]`.
    #[tokio::test]
    async fn an_unrecognised_response_exits_internal() {
        let (code, out, err, _envelopes) = run(&args("repl", "gc")).await;
        assert_eq!(code, ExitCode::Internal);
        assert!(out.is_empty());
        assert!(!err.is_empty());
    }

    /// fails if a `NotFound` reply is swallowed. A selector that matched no
    /// registered sheep is the one way this verb fails as a whole request —
    /// distinct from a matched sheep with no pipe, which is a `no_stdin` ROW.
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
                style: crate::style::Presentation::BARE,
            };
            whisper(&client, &mut streams, Format::Table, &args("ghost", "gc")).await
        };
        assert_eq!(code, ExitCode::NotFound);
        assert!(out.is_empty());
    }
}
