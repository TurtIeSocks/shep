//! `save`: write the muster roll now, bypassing the snapshot writer's
//! debounce.
//!
//! `Request::SaveRoll` carries no fields — unlike every verb in
//! `commands::lifecycle`/`commands::query`, this one has nothing to target:
//! the roll always records the whole flock, so there is no `SelectorArgs`
//! for this module to parse. One request, one call site, so — matching
//! `trigger`'s own reasoning — this inlines the match rather than routing
//! through `commands::query`'s shared `request_and_render` helper, which
//! nothing else here would call.

use shep_client::Client;
use shep_core::protocol::{Request, Response};

use crate::cli::Format;
use crate::exit::ExitCode;
use crate::output::{SavedRollRow, Streams, emit, emit_error, write_outcome};

/// Asks the daemon to write the muster roll now, and reports where it
/// landed and how many apps it recorded.
///
/// `shep save` exists so an operator knows the roll is on disk before a
/// reboot — a failed save is loud on purpose, never a silent no-op.
pub async fn save(client: &Client, streams: &mut Streams<'_>, fmt: Format) -> ExitCode {
    match client.request(Request::SaveRoll).await {
        Ok(Response::RollSaved { path, apps }) => write_outcome(emit(
            &mut *streams.out,
            fmt,
            "save",
            SavedRollRow { file: path, apps },
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
    use std::time::Duration;

    use shep_client::testing::{fake_client_capturing_envelopes, fake_client_replying_err};
    use shep_core::protocol::RpcErrorCode;

    use super::*;

    /// Every `envelopes.recv()` in this module is bounded by this timeout
    /// rather than left to run to completion — `commands::query`'s own
    /// `RECV_TIMEOUT` carries the reason: a test meant to catch a mutation
    /// that skips the request entirely must fail with a named assertion,
    /// not hang.
    const FAKE_REPLY_WAIT: Duration = Duration::from_secs(5);

    /// fails if `save` sends anything but `SaveRoll` — a verb wired to
    /// `ListFlock` still gets a reply from the fake daemon and would pass
    /// every other assertion here.
    #[tokio::test]
    async fn save_sends_save_roll_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };
        let _ = save(&client, &mut streams, Format::Table).await;

        let envelope = tokio::time::timeout(FAKE_REPLY_WAIT, envelopes.recv())
            .await
            .expect("the fake daemon must answer inside the bound")
            .unwrap();
        assert_eq!(envelope.body, Request::SaveRoll);
    }

    /// fails if `save` treats an RPC failure as a success. `shep save`
    /// exists so a failed save is loud; an exit 0 here is the bug the verb
    /// was added to make impossible.
    #[tokio::test]
    async fn a_failed_save_exits_non_zero_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, _served) = fake_client_replying_err(
            &path,
            RpcErrorCode::Internal,
            "the supervisor engine has stopped; no roll was written",
        )
        .await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            save(&client, &mut streams, Format::Table).await
        };
        assert_eq!(code, ExitCode::Internal);
        assert!(out.is_empty(), "a failed save prints no success table");
        assert!(String::from_utf8(err).unwrap().contains("engine"));
    }
}
