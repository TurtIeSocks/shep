//! `signal`: send a unix signal to matched sheep's own processes, never
//! their process group.
//!
//! One verb, one call site, following `trigger`'s own precedent: inlining the
//! one match this needs reads more plainly than a shared generic wrapper
//! nothing else here would call.
//!
//! The signal name is validated locally, before the round trip — a malformed
//! name is a usage error the operator caused, and it should cost neither a
//! connection nor a daemon round trip. The daemon re-validates anyway
//! (`shep-daemon`'s own `Request::Signal` arm), because peer input is
//! untrusted and a client is not the only thing that can send a frame.
//!
//! Sent with the client's plain default deadline: nothing on this path waits
//! on an app the way `trigger` waits on its `action_timeout` — a `kill(2)`
//! either returns or does not.
//!
//! A row's own outcome — `Delivered`, `NotRunning`, `Failed` — is never a
//! request failure: the daemon reports it per matched sheep, following
//! `Trigger`/`Reopen`/`Flush`'s own precedent, so `shep signal` exits
//! non-`Success` only when the RPC itself failed (a malformed selector or
//! signal name caught locally, a selector that matched nothing, a daemon this
//! client could not reach). What each row says is `SignalledRows`'s job
//! (`output/rows.rs`), not this module's.

use shep_client::Client;
use shep_core::protocol::{Request, Response, SelectorSpec};
use shep_core::signals::OperatorSignal;

use crate::cli::{Format, SignalArgs};
use crate::commands::selector::parse_selector;
use crate::exit::ExitCode;
use crate::output::{SignalledRows, Streams, emit, emit_error, write_outcome};

/// Sends `args.signal` to the sheep matching `args.selector`, and renders one
/// row per match.
pub async fn signal(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    args: &SignalArgs,
) -> ExitCode {
    let selector = match parse_selector(streams, fmt, &args.selector) {
        Ok(selector) => SelectorSpec::from(&selector),
        Err(code) => return code,
    };

    let Some(sig) = OperatorSignal::parse(&args.signal) else {
        let message = format!(
            "`{}` is not a signal shep will send; accepted: {}",
            args.signal,
            OperatorSignal::ACCEPTED.join(", ")
        );
        let _ = emit_error(&mut *streams.err, fmt, ExitCode::Usage.code_str(), &message);
        return ExitCode::Usage;
    };

    let body = Request::Signal {
        selector,
        // Canonical spelling, not `args.signal` verbatim: the wire carries
        // `SIGHUP` whether the operator typed `hup`, `Hup` or `SIGHUP`, so a
        // packet capture reads the same for all three.
        signal: sig.as_str().to_string(),
    };

    match client.request(body).await {
        Ok(Response::Signalled(replies)) => write_outcome(emit(
            &mut *streams.out,
            fmt,
            "signal",
            SignalledRows(replies),
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

    fn args(selector: &str, signal: &str) -> SignalArgs {
        SignalArgs {
            selector: selector.to_string(),
            signal: signal.to_string(),
        }
    }

    /// `"/[/"` is one of the only three inputs the selector grammar rejects.
    /// A verb that skipped the client-side parse would send it and exit
    /// `NotFound` instead of `Usage`, and the daemon would see a request it
    /// never should have. Checked ahead of the signal name (`parse_selector`
    /// runs first in `signal`'s own body), so a malformed selector never even
    /// reaches signal-name validation.
    #[tokio::test]
    async fn a_malformed_selector_is_a_local_usage_error() {
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
            signal(&client, &mut streams, Format::Table, &args("/[/", "hup")).await
        };
        assert_eq!(code, ExitCode::Usage);
        assert!(
            envelopes.try_recv().is_err(),
            "a malformed selector must fail locally, never reach the wire"
        );
    }

    /// A bad signal name never reaches the wire and exits `Usage`, naming the
    /// accepted list so the operator can fix it without a daemon round trip.
    #[tokio::test]
    async fn a_bad_signal_name_never_reaches_the_wire() {
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
            signal(
                &client,
                &mut streams,
                Format::Table,
                &args("web", "SIGHUPP"),
            )
            .await
        };
        assert_eq!(code, ExitCode::Usage);
        assert!(
            envelopes.try_recv().is_err(),
            "a malformed signal name must fail locally, never reach the wire"
        );
        assert!(out.is_empty());
        assert!(!err.is_empty());
    }

    /// A selector that matched no registered sheep is the one way `signal`
    /// fails as a whole request — distinct from a matched sheep with no live
    /// process, which is a `NotRunning` *row*, not a failure.
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
            signal(
                &client,
                &mut streams,
                Format::Table,
                &args("nonexistent", "hup"),
            )
            .await
        };
        assert_eq!(code, ExitCode::NotFound);
        assert!(out.is_empty());
    }

    /// The envelope's own `body`: a selector converted with the wrong helper,
    /// or a signal name sent verbatim instead of canonicalised, would still
    /// get a reply from the fake daemon (it always answers `Pong`) and only
    /// this catches them. Lowercase input on purpose — the canonical-spelling
    /// assertion is meaningless against an already-canonical one.
    #[tokio::test]
    async fn the_request_carries_the_selector_and_the_canonical_signal_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };
        let _ = signal(&client, &mut streams, Format::Table, &args("web", "hup")).await;

        let envelope = envelopes.recv().await.unwrap();
        assert_eq!(
            envelope.body,
            Request::Signal {
                selector: SelectorSpec::Name("web".to_string()),
                signal: "SIGHUP".to_string(),
            }
        );
    }

    /// A response this client does not recognise (the fake daemon's generic
    /// `Pong`, standing in for a `Response` variant `signal`'s `match` has no
    /// arm for) must not be read as any of the three outcomes — it is
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
            signal(&client, &mut streams, Format::Table, &args("web", "hup")).await
        };
        assert_eq!(code, ExitCode::Internal);
        assert!(out.is_empty());
        assert!(!err.is_empty());
    }
}
