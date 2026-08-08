//! Query verbs: `flock`, `describe`, `fold`, `ping` — the read-only half of
//! the verb set. None of them mutate the flock, and none of them autostart:
//! `main` hands every one of these an already-connected [`Client`], the
//! same contract `commands::lifecycle` documents on its own module.
//!
//! `describe` and `fold` share one shape (`Request::Describe` against a
//! [`SelectorSpec`]) — `fold` is a one-line specialisation of `describe`
//! that supplies `SelectorSpec::Fold` directly rather than parsing one from
//! a raw string, kept as a thin wrapper rather than a copy.
//!
//! `ping` deliberately does not ask the daemon for its own version and pid:
//! the handshake already answered that, in the
//! [`HelloAck`](shep_core::protocol::HelloAck) [`Client::daemon`] holds, so
//! asking again would be a wasted round trip for information already in
//! hand. It still issues `Request::Ping` itself, as the liveness check the
//! verb exists for.

use shep_client::Client;
use shep_core::protocol::{Request, Response, SelectorSpec};
use shep_core::selector::ProcessSelector;

use crate::cli::{FoldArgs, Format, SelectorArgs};
use crate::exit::ExitCode;
use crate::output::{FlockRows, PingRow, Render, Streams, emit, emit_error, write_outcome};

/// Sends `body`, renders whatever the daemon answers through [`emit`], and
/// maps every way that can go wrong to its exit code.
///
/// `extract` pulls the verb's own payload out of `Response`; `Response` is
/// `#[non_exhaustive]` (Global Constraints), so an answer `extract` does not
/// recognise — a variant this client predates, or simply the wrong one for
/// this verb — maps to [`ExitCode::Internal`] rather than being guessed at.
/// Every query verb uses the client's default deadline, so unlike
/// `commands::lifecycle`'s version of this helper there is no deadline
/// parameter to thread through.
async fn request_and_render<T, F>(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    command: &str,
    body: Request,
    extract: F,
) -> ExitCode
where
    T: Render,
    F: FnOnce(Response) -> Option<T>,
{
    match client.request(body).await {
        Ok(response) => match extract(response) {
            Some(payload) => write_outcome(emit(&mut *streams.out, fmt, command, payload)),
            None => {
                let message = "the daemon answered with a response this client does not understand";
                let _ = emit_error(
                    &mut *streams.err,
                    fmt,
                    ExitCode::Internal.code_str(),
                    message,
                );
                ExitCode::Internal
            }
        },
        Err(err) => {
            let code = ExitCode::from(&err);
            let _ = emit_error(&mut *streams.err, fmt, code.code_str(), &err.to_string());
            code
        }
    }
}

/// Parses `raw` client-side, so a malformed selector is a fast local usage
/// error rather than a round trip to the daemon (the daemon re-parses it
/// too, but only after this one already succeeded). The same reasoning
/// `commands::lifecycle::parse_selector` documents on its own copy — kept
/// as a small per-module duplicate rather than a shared abstraction, since
/// there is no third caller yet to justify one.
fn parse_selector(
    streams: &mut Streams<'_>,
    fmt: Format,
    raw: &str,
) -> Result<SelectorSpec, ExitCode> {
    match ProcessSelector::parse(raw) {
        Ok(selector) => Ok(SelectorSpec::from(&selector)),
        Err(err) => {
            let _ = emit_error(
                &mut *streams.err,
                fmt,
                ExitCode::Usage.code_str(),
                &err.to_string(),
            );
            Err(ExitCode::Usage)
        }
    }
}

/// `describe` and `fold`'s shared body: one `Request::Describe` against
/// `selector`, rendered as [`FlockRows`]. `command` is the verb name the
/// output envelope reports (`"describe"` or `"fold"`), which is why this
/// takes it as a parameter rather than hard-coding one.
async fn describe_selector(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    command: &str,
    selector: SelectorSpec,
) -> ExitCode {
    request_and_render(
        client,
        streams,
        fmt,
        command,
        Request::Describe { selector },
        |response| match response {
            Response::Described(procs) => Some(FlockRows(procs)),
            _ => None,
        },
    )
    .await
}

/// Lists the whole flock.
pub async fn flock(client: &Client, streams: &mut Streams<'_>, fmt: Format) -> ExitCode {
    request_and_render(
        client,
        streams,
        fmt,
        "flock",
        Request::ListFlock,
        |response| match response {
            Response::Flock(procs) => Some(FlockRows(procs)),
            _ => None,
        },
    )
    .await
}

/// Describes the sheep matching `args.selector` in detail.
pub async fn describe(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    args: &SelectorArgs,
) -> ExitCode {
    let selector = match parse_selector(streams, fmt, &args.selector) {
        Ok(selector) => selector,
        Err(code) => return code,
    };
    describe_selector(client, streams, fmt, "describe", selector).await
}

/// Lists one fold (spec §5 / §9): `Request::Describe { selector:
/// SelectorSpec::Fold(args.name) }`, delegating straight to
/// [`describe_selector`] rather than re-implementing the request/render
/// shape.
pub async fn fold(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    args: &FoldArgs,
) -> ExitCode {
    describe_selector(
        client,
        streams,
        fmt,
        "fold",
        SelectorSpec::Fold(args.name.clone()),
    )
    .await
}

/// Checks whether the shepherd answers.
///
/// Issues `Request::Ping` as the liveness check — a reply of any kind means
/// the daemon is alive — but sources `daemon_version` and `pid` from the
/// [`shep_core::protocol::HelloAck`] the handshake already produced
/// ([`Client::daemon`]), not from the `Pong` reply, which carries neither.
pub async fn ping(client: &Client, streams: &mut Streams<'_>, fmt: Format) -> ExitCode {
    let ack = client.daemon();
    let daemon_version = ack.daemon_version.clone();
    let pid = ack.pid;
    request_and_render(
        client,
        streams,
        fmt,
        "ping",
        Request::Ping,
        |response| match response {
            Response::Pong => Some(PingRow {
                daemon_version,
                pid,
            }),
            _ => None,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use shep_client::testing::{fake_client_capturing_envelopes, fake_client_with_ack};
    use shep_core::protocol::{HelloAck, PROTOCOL_VERSION};

    use super::*;

    /// `flock` must send `Request::ListFlock` and nothing else — the same
    /// class of guard Task 8's reviewer found missing for `restart` and
    /// `delete` (mutating either to send `Request::Stop` left every test in
    /// that module green). An implementation that sent `Request::Describe`
    /// (e.g. copy-pasted from `describe`) fails this.
    #[tokio::test]
    async fn flock_asks_the_daemon_to_list_the_whole_flock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };
        let _ = flock(&client, &mut streams, Format::Table).await;
        let sent = envelopes.recv().await.unwrap();
        assert_eq!(sent.body, Request::ListFlock);
    }

    /// The client-side parse is the point: `describe` must send a compiled
    /// `SelectorSpec` inside `Request::Describe`, not the raw string and not
    /// `SelectorSpec::All` regardless of input. Without this, a `describe`
    /// that always sent `SelectorSpec::All` would pass every other test in
    /// this module.
    #[tokio::test]
    async fn describe_sends_the_parsed_selector_in_its_compiled_form() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;

        for (input, expected) in [
            ("all", SelectorSpec::All),
            ("7", SelectorSpec::Id(7)),
            ("web", SelectorSpec::Name("web".into())),
            ("/^web-/", SelectorSpec::Regex("^web-".into())),
            ("fold:api", SelectorSpec::Fold("api".into())),
        ] {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            let args = SelectorArgs {
                selector: input.into(),
            };
            let _ = describe(&client, &mut streams, Format::Table, &args).await;
            let sent = envelopes.recv().await.unwrap();
            assert_eq!(
                sent.body,
                Request::Describe { selector: expected },
                "{input}"
            );
        }
    }

    /// `"/[/"` is one of the only three inputs the selector grammar rejects
    /// (an unterminated regex character class). A `describe` that skipped
    /// the client-side parse would send it to the daemon and exit
    /// `NotFound` instead of failing locally.
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
            describe(
                &client,
                &mut streams,
                Format::Table,
                &SelectorArgs {
                    selector: "/[/".into(),
                },
            )
            .await
        };
        assert_eq!(code, ExitCode::Usage);
        assert!(
            envelopes.try_recv().is_err(),
            "a malformed selector must fail locally"
        );
    }

    /// `fold <name>` must ask for exactly that fold, via `Request::Describe
    /// { selector: SelectorSpec::Fold(name) }` — not `describe`'s selector
    /// grammar, and not a wider `SelectorSpec::All`.
    #[tokio::test]
    async fn fold_asks_the_daemon_for_that_fold_and_nothing_wider() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };
        let _ = fold(
            &client,
            &mut streams,
            Format::Table,
            &FoldArgs { name: "api".into() },
        )
        .await;
        let sent = envelopes.recv().await.unwrap();
        assert_eq!(
            sent.body,
            Request::Describe {
                selector: SelectorSpec::Fold("api".into())
            }
        );
    }

    /// The fake daemon acks with a distinctive version and pid, then replies
    /// `Pong` — which carries neither. A `ping` that sourced either from the
    /// reply has nothing to source them FROM, so it would emit defaults or
    /// panic.
    #[tokio::test]
    async fn ping_reads_version_and_pid_from_the_handshake_not_from_a_reply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let ack = HelloAck {
            daemon_version: "9.9.9".into(),
            protocol: PROTOCOL_VERSION,
            pid: 4242,
        };
        let (client, _daemon) = fake_client_with_ack(&path, ack).await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            ping(&client, &mut streams, Format::Json).await
        };

        assert_eq!(code, ExitCode::Success);
        let json: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(json["data"]["daemon_version"], "9.9.9");
        assert_eq!(json["data"]["pid"], 4242);
    }

    /// `ping` must still round-trip a real `Request::Ping` — sourcing the
    /// version and pid from the handshake is not a licence to skip the
    /// liveness check the verb exists for.
    #[tokio::test]
    async fn ping_still_issues_the_liveness_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };
        let _ = ping(&client, &mut streams, Format::Table).await;
        assert_eq!(envelopes.recv().await.unwrap().body, Request::Ping);
    }
}
