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

use crate::cli::{FoldArgs, Format, SelectorArgs};
use crate::commands::selector::parse_selector;
use crate::exit::ExitCode;
use crate::output::{
    DogRows, PingRow, Render, Streams, emit, emit_described, emit_error, emit_flock, write_outcome,
};

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

/// `describe` and `fold`'s shared body: one `Request::Describe` against
/// `selector`, rendered through [`emit_described`] — the sheep table, then
/// each sheep's lamb tree beneath it. `command` is the verb name the output
/// envelope reports (`"describe"` or `"fold"`), which is why this takes it
/// as a parameter rather than hard-coding one.
///
/// Not routed through [`request_and_render`], for the same reason
/// [`flock`] is not: `emit_described` renders one `Vec<ProcessInfo>` into
/// two tables in table mode, which no single [`Render`] impl can express —
/// this is the small bespoke path this verb needs instead of widening that
/// helper into a second renderer.
async fn describe_selector(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    command: &str,
    selector: SelectorSpec,
) -> ExitCode {
    match client.request(Request::Describe { selector }).await {
        Ok(Response::Described(procs)) => {
            write_outcome(emit_described(&mut *streams.out, fmt, command, procs))
        }
        Ok(_) => {
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

/// Lists the whole flock: the sheep table, then the dogs table beneath it
/// whenever any dog is registered — [`emit_flock`]'s own job.
///
/// Not routed through [`request_and_render`]: that helper renders exactly
/// one [`Render`] type per verb, through [`emit`]. A flock listing renders
/// through two tables built from one `Vec<ProcessInfo>`, which no single
/// `Render` impl can express — `emit_flock` is the small path this verb
/// needs instead of widening that helper into a second renderer, keeping
/// the same connect/request/extract/render shape every other query verb
/// here has.
pub async fn flock(client: &Client, streams: &mut Streams<'_>, fmt: Format) -> ExitCode {
    match client.request(Request::ListFlock).await {
        Ok(Response::Flock(procs)) => {
            write_outcome(emit_flock(&mut *streams.out, fmt, "flock", procs))
        }
        Ok(_) => {
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

/// Lists the dogs, and nothing else: the same `Request::ListFlock` `flock`
/// sends, filtered to the entries carrying a `dog` marker and rendered as
/// [`DogRows`] through the ordinary [`request_and_render`] path every other
/// query verb here uses — `DogRows` is a [`Render`] impl like any other, so
/// no bespoke render path is needed the way `flock`'s own split is.
///
/// Deliberately not [`emit_flock`]: that function's contract is a MIXED
/// listing, sheep table first: handing it a dogs-only `Vec<ProcessInfo>`
/// would still print the sheep table's header row for zero sheep, which is
/// exactly what this verb's own doc comment (`Commands::Dogs`, "and nothing
/// else") rules out. The two call sites still share one table renderer —
/// `render_table::<DogRows>`, reached here through `emit` and from inside
/// `emit_flock`'s own dogs section — so there is exactly one place that
/// knows how to lay out a dogs table.
pub async fn dogs(client: &Client, streams: &mut Streams<'_>, fmt: Format) -> ExitCode {
    request_and_render(
        client,
        streams,
        fmt,
        "dogs",
        Request::ListFlock,
        |response| match response {
            Response::Flock(procs) => Some(DogRows(
                procs.into_iter().filter(|p| p.dog.is_some()).collect(),
            )),
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
        Ok(selector) => SelectorSpec::from(&selector),
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
    use std::time::Duration;

    use shep_client::testing::{
        fake_client_capturing_envelopes, fake_client_with_ack, sample_ack, sample_info,
    };
    use shep_core::protocol::{HelloAck, PROTOCOL_VERSION};

    use super::*;

    /// Every `envelopes.recv()` in this module is bounded by this timeout
    /// rather than left to run to completion — the same rule
    /// `commands::lifecycle`'s tests apply
    /// (`crates/shep-cli/src/commands/lifecycle.rs:613,652`) and this module
    /// originally skipped: a Task 9 reviewer mutated `ping` to render from
    /// `client.daemon()` without issuing the request, and the test meant to
    /// catch that hung past 90 seconds instead of failing. A test that fails
    /// by hanging gives CI a killed job, not a named assertion.
    const RECV_TIMEOUT: Duration = Duration::from_secs(5);

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
        let sent = tokio::time::timeout(RECV_TIMEOUT, envelopes.recv())
            .await
            .expect("flock must reach the wire; it hung instead of sending a request")
            .unwrap();
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
            let sent = tokio::time::timeout(RECV_TIMEOUT, envelopes.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!("describe({input}) must reach the wire; it hung instead of sending a request")
                })
                .unwrap();
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
        let sent = tokio::time::timeout(RECV_TIMEOUT, envelopes.recv())
            .await
            .expect("fold must reach the wire; it hung instead of sending a request")
            .unwrap();
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
        let sent = tokio::time::timeout(RECV_TIMEOUT, envelopes.recv())
            .await
            .expect("ping must reach the wire; it hung instead of sending a request")
            .unwrap();
        assert_eq!(sent.body, Request::Ping);
    }

    /// Proves the `Response::Flock` arm inside `flock`'s own `extract`
    /// closure is wired to the right variant, not merely that some payload
    /// renders as `FlockRows` in isolation (`output/rows.rs`'s own tests
    /// already cover that) and not merely that the right request went out
    /// (`flock_asks_the_daemon_to_list_the_whole_flock`, above, covers
    /// that). `Response::Flock` and `Response::Described` both wrap a bare
    /// `Vec<ProcessInfo>`, so a match arm swapped between them compiles
    /// clean and passes every other test in this file — this is the one
    /// that fails: `FakeDaemon::reply_to_list` scripts a real
    /// `Response::Flock`, not the `Response::Pong` every other fake in this
    /// module hands back regardless of what was sent.
    #[tokio::test]
    async fn flock_response_round_trips_into_rendered_flock_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, daemon) = fake_client_with_ack(&path, sample_ack()).await;
        daemon.reply_to_list(vec![sample_info()]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            flock(&client, &mut streams, Format::Json).await
        };

        assert_eq!(code, ExitCode::Success);
        let json: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(json["command"], "flock");
        assert_eq!(json["data"][0]["name"], "web");
    }

    /// Proves the `Response::Described` arm inside `describe_selector`'s
    /// `extract` closure is wired to the right variant, by the same
    /// reasoning as the test above — and, since `fold` delegates straight
    /// to `describe_selector` rather than duplicating it, this covers
    /// `fold`'s wiring too without a third fake-daemon round trip. Also
    /// covers Minor 5: `describe` and `fold` share this one code path and
    /// this one `command` string parameter, so an envelope with `"describe"`
    /// and `"fold"` swapped would otherwise go unasserted.
    #[tokio::test]
    async fn describe_response_round_trips_into_rendered_flock_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, daemon) = fake_client_with_ack(&path, sample_ack()).await;
        daemon.reply_to_describe(vec![sample_info()]);

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
                Format::Json,
                &SelectorArgs {
                    selector: "all".into(),
                },
            )
            .await
        };

        assert_eq!(code, ExitCode::Success);
        let json: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(json["command"], "describe");
        assert_eq!(json["data"][0]["name"], "web");
    }
}
