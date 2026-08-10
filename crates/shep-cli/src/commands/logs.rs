//! Log-plane verbs: the ones that act on a sheep's log FILES.
//!
//! Reading a sheep's log output is `bleats` and lives in
//! `commands::bleats` — which is what `shep logs` aliases, despite this
//! module's name. This module is the other half: the files themselves, and
//! what an external rotator needs the daemon to do to them.
//!
//! Like every other verb module, these receive an already-connected
//! [`Client`]; `main` connects, and nothing here autostarts a daemon.
//!
//! # Why reopening is cheap
//!
//! The child never sees its log file. It is spawned with `Stdio::piped()`
//! and the daemon does the file I/O on the far side of that pipe, so
//! swapping the daemon's handle is invisible across the process boundary:
//! no signal to the child, no fd surgery, no restart, and no gap in the
//! pipe. Nothing child-side is needed to rotate a sheep's logs, so nothing
//! here asks anything of the child.

use std::time::Duration;

use shep_client::{Client, REOPEN_DEADLINE};
use shep_core::protocol::{Request, Response, SelectorSpec};
use shep_core::selector::ProcessSelector;

use crate::cli::{Format, ReopenArgs};
use crate::exit::ExitCode;
use crate::output::{FlockRows, Render, Streams, emit, emit_error, write_outcome};

/// Sends `body` with `deadline` (`None` defers to the client's own default),
/// renders whatever the daemon answers through [`emit`], and maps every way
/// that can go wrong to its exit code.
///
/// `extract` pulls the verb's own payload out of `Response`; `Response` is
/// `#[non_exhaustive]` (Global Constraints), so an answer `extract` does not
/// recognise — a variant this client predates, or simply the wrong one for
/// this verb — maps to [`ExitCode::Internal`] rather than being guessed at.
///
/// The third per-module copy of this helper, after `commands::lifecycle`'s
/// and `commands::query`'s. They are one refactor rather than three: this
/// one and `lifecycle`'s are now identical, and `query`'s differs only by
/// the deadline parameter it has no verb to use. Kept a copy here because
/// pulling all three into a shared home rewrites two modules this change
/// otherwise does not touch.
async fn request_and_render<T, F>(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    command: &str,
    body: Request,
    deadline: Option<Duration>,
    extract: F,
) -> ExitCode
where
    T: Render,
    F: FnOnce(Response) -> Option<T>,
{
    match client.request_with_deadline(body, deadline).await {
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
/// too, but only after this one already succeeded). The same per-module
/// copy `commands::lifecycle` and `commands::query` each carry.
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

/// Reopens the log files of the sheep matching `args.selector`, for an
/// external rotator that has renamed them.
///
/// A zero exit means every matched sheep's log pump holds a handle on the
/// recreated path, so a `postrotate` stanza that waits for this command
/// knows no live pump is still filling the archive it just renamed. A
/// matched sheep that is not running has no pump and nothing to reopen; it
/// is reported alongside the rest rather than as a failure.
///
/// A pump that could not open a path again fails the command instead, with
/// the sheep and the path on stderr. The rename is still safe to act on —
/// the old handle was closed either way — but that sheep is writing a
/// stream nowhere until the path can be opened, and exiting 0 there would
/// be the silent failure this verb exists to end.
///
/// Renders the matched sheep as [`FlockRows`], the same table `stop` and
/// `restart` answer with — the useful thing to show is which sheep the
/// selector reached.
///
/// Sent with [`REOPEN_DEADLINE`] rather than the client's default, the way
/// `lifecycle::start` sends its own: the daemon visits matched sheep one at
/// a time with no per-sheep bound, so on a slow log directory the 5s default
/// would hand a `postrotate` stanza a `DeadlineExceeded` for a reopen that
/// was still running.
pub async fn reopen(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    args: &ReopenArgs,
) -> ExitCode {
    let selector = match parse_selector(streams, fmt, &args.selector) {
        Ok(selector) => selector,
        Err(code) => return code,
    };
    request_and_render(
        client,
        streams,
        fmt,
        "reopen",
        Request::Reopen { selector },
        Some(REOPEN_DEADLINE),
        |response| match response {
            Response::Reopened(procs) => Some(FlockRows(procs)),
            _ => None,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use shep_client::testing::{fake_client_capturing_envelopes, fake_client_replying_err};
    use shep_core::protocol::RpcErrorCode;

    fn args(selector: &str) -> ReopenArgs {
        ReopenArgs {
            selector: selector.to_string(),
        }
    }

    /// Fails if `reopen` sends the raw selector string, another verb's
    /// request kind, or a selector it did not parse: the whole `sent.body`
    /// is asserted, not just the selector inside it. A `reopen` wired to
    /// `Request::Restart` would restart the flock on every rotation — the
    /// most expensive way this verb can be wrong, and invisible to a test
    /// that only checked the selector.
    #[tokio::test]
    async fn every_selector_form_reaches_the_wire_inside_a_reopen_request() {
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
            let _ = reopen(&client, &mut streams, Format::Table, &args(input)).await;
            let sent = envelopes.recv().await.unwrap();
            assert_eq!(
                sent.body,
                Request::Reopen { selector: expected },
                "input={input}"
            );
        }
    }

    /// Fails if `reopen` leaves the deadline to the client's default. The
    /// daemon visits matched sheep serially with no per-sheep bound, so on a
    /// slow or NFS-backed log directory a 5s budget expires while the reopen
    /// is still running — and the one caller the docs invite to wait for
    /// this, a logrotate `postrotate` stanza, gets both a non-zero exit and
    /// pumps still holding the inodes it renamed.
    ///
    /// Asserted on the wire rather than on the constant: `deadline_ms` is
    /// what the daemon actually budgets from, and `request_with_deadline`
    /// never leaves it unset — `None` would travel as
    /// `DEFAULT_DEADLINE`'s 5s, which is exactly the regression.
    #[tokio::test]
    async fn a_reopen_asks_for_the_longer_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };

        let _ = reopen(&client, &mut streams, Format::Table, &args("all")).await;

        let sent = envelopes.recv().await.unwrap();
        assert_eq!(
            sent.deadline_ms,
            Some(u64::try_from(REOPEN_DEADLINE.as_millis()).unwrap())
        );
    }

    /// `"/[/"` is one of the only three inputs the selector grammar
    /// rejects. Fails if `reopen` skips the client-side parse: the daemon
    /// would answer `NotFound` after a round trip instead.
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
            reopen(&client, &mut streams, Format::Table, &args("/[/")).await
        };
        assert_eq!(code, ExitCode::Usage);
        assert!(
            envelopes.try_recv().is_err(),
            "a malformed selector must fail locally"
        );
    }

    /// Fails if the verb swallows a daemon-side refusal and exits 0. The
    /// selector matching nothing is the one refusal `reopen` can provoke on
    /// its own, since no other input reaches the daemon.
    #[tokio::test]
    async fn a_not_found_reply_exits_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, _served) =
            fake_client_replying_err(&path, RpcErrorCode::NotFound, "no sheep matched").await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };
        let code = reopen(&client, &mut streams, Format::Table, &args("ghost")).await;
        assert_eq!(code, ExitCode::NotFound);
    }
}
