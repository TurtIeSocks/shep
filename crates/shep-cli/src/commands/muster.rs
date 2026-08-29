//! `save`/`muster`: write the muster roll now, and assemble the flock from
//! it on demand.
//!
//! `Request::SaveRoll` and `Request::Muster` both carry no fields — unlike
//! every verb in `commands::lifecycle`/`commands::query`, neither has
//! anything to target: the roll always records (or restores) the whole
//! flock, so there is no `SelectorArgs` for this module to parse. Two
//! requests, two call sites, each small enough that — matching `trigger`'s
//! own reasoning — this inlines its own match rather than routing through
//! `commands::query`'s shared `request_and_render` helper. `muster` also
//! cannot reuse `commands::lifecycle`'s own version of that helper: it
//! needs the empty-roll notice below, which no other verb does.

use shep_client::{Client, START_DEADLINE};
use shep_core::protocol::{Request, Response};
use shep_core::status::ProcStatus;

use crate::cli::Format;
use crate::exit::ExitCode;
use crate::flourish;
use crate::output::{FlockRows, SavedRollRow, Streams, emit, write_outcome};

/// Asks the daemon to write the muster roll now, and reports where it
/// landed and how many apps it recorded.
///
/// `shep save` exists so an operator knows the roll is on disk before a
/// reboot — a failed save is loud on purpose, never a silent no-op.
pub async fn save(client: &Client, streams: &mut Streams<'_>) -> ExitCode {
    match client.request(Request::SaveRoll).await {
        Ok(Response::RollSaved { path, apps }) => write_outcome(emit(
            &mut *streams.out,
            streams.fmt,
            "save",
            SavedRollRow { file: path, apps },
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

/// Asks the daemon to assemble the flock from the muster roll `save` wrote,
/// and reports who came up.
///
/// Sent with `START_DEADLINE` rather than the client's plain 5s default —
/// the same reasoning `lifecycle::start` already established: a muster
/// spawns every app in the roll, and a cold restore of a real flock
/// routinely outruns five seconds. A client-side abandonment there would
/// report failure for a flock that came up fine.
///
/// `main`'s dispatch reaches this verb through `connect_or_spawn_client`,
/// not `connect_client` — the second autostart path in the binary, after
/// `start` (see `main::run`'s own doc). When that call actually spawns the
/// daemon, boot has already restored the roll before this request is even
/// sent, so the `Muster` that follows spawns nothing new — `do_start` is
/// idempotent through `instance_slots` — and simply reports the flock that
/// restore produced. That is the cutover design doing its job, not a wasted
/// round trip (`docs/decisions.md`, "The pm2 cutover"):
/// `Response::Mustered` always names every sheep of every app the
/// roll restored, not only the ones this particular call spawned, which is
/// what makes the verb safe to run more than once — an init system that
/// calls it twice gets the same honest answer both times.
///
/// An empty `Mustered` — the roll restored nothing — gets an explicit
/// notice on stderr in addition to the (empty) table: "the roll restored
/// nothing" is the answer an operator needs most, and the one a quiet exit
/// 0 hides.
///
/// A non-empty `Mustered` gets [`flourish::mustered`] after the table
/// instead, built from every restored sheep's real status
/// (`procs.iter().map(|p| p.status)`), never from a bare count: a stopped
/// sheep stays a member of the flock across a restart, so mustering an
/// already-stopped roll restores it without starting it, and the table
/// says `stopped` right above where the flourish prints -- the two must
/// never disagree (fix round 1). Unlike `query::flock`'s empty/all-asleep
/// flourishes, which answer "what now" before the receipt, this one is a
/// milestone reached after a restore that just happened, so it reads as
/// the last line of the story rather than the first. `Response::Mustered`
/// carries no dogs of its own to filter — it is the roll's own apps,
/// filtered by name in `rpc.rs`'s handler — so, unlike
/// `query::sheep_flourish`, there is no dog/sheep split to make here.
/// Gated the same way every other flourish is: `Format::Table` and
/// `streams.style.level.sheep()` only, so `--format json` and a piped
/// table are unchanged.
pub async fn muster(client: &Client, streams: &mut Streams<'_>) -> ExitCode {
    match client
        .request_with_deadline(Request::Muster, Some(START_DEADLINE))
        .await
    {
        Ok(Response::Mustered(procs)) => {
            if procs.is_empty() {
                streams.aside(
                    "muster_restored_nothing",
                    "the muster roll restored nothing",
                );
            }
            // Read before `procs` moves into `FlockRows`: the flourish is
            // built from every restored sheep's real status, never from a
            // bare count, so it can never disagree with the table it sits
            // beneath (fix round 1 -- an earlier version rendered
            // `ProcStatus::Online` regardless of what actually came back).
            let statuses: Vec<ProcStatus> = procs.iter().map(|p| p.status).collect();
            let outcome = write_outcome(emit(
                &mut *streams.out,
                streams.fmt,
                "muster",
                FlockRows(procs),
                streams.style,
            ));
            if streams.fmt == Format::Table && !statuses.is_empty() && streams.style.level.sheep() {
                let _ = write!(streams.out, "{}", flourish::mustered(&statuses));
            }
            outcome
        }
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
    use std::time::Duration;

    use shep_client::testing::{
        fake_client_capturing_envelopes, fake_client_on, fake_client_replying_err, sample_info,
    };
    use shep_core::protocol::{BusEvent, ProcessEventKind, RpcErrorCode};

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
        let _ = save(&client, &mut streams).await;

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
        let path = shep_client::testing::control_address(dir.path());
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
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            save(&client, &mut streams).await
        };
        assert_eq!(code, ExitCode::Internal);
        assert!(out.is_empty(), "a failed save prints no success table");
        assert!(String::from_utf8(err).unwrap().contains("engine"));
    }

    /// fails if `muster` sends anything but `Muster`, or asks for the
    /// client's plain 5s default. A cold restore of a real flock routinely
    /// outruns five seconds, and a client-side abandonment there reports
    /// failure for a flock that came up fine.
    #[tokio::test]
    async fn muster_sends_muster_with_the_start_deadline() {
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
        let _ = muster(&client, &mut streams).await;

        let envelope = tokio::time::timeout(FAKE_REPLY_WAIT, envelopes.recv())
            .await
            .expect("the fake daemon must answer inside the bound")
            .unwrap();
        assert_eq!(envelope.body, Request::Muster);
        assert_eq!(
            envelope.deadline_ms,
            Some(u64::try_from(START_DEADLINE.as_millis()).unwrap())
        );
    }

    /// fails if an empty muster prints an empty table and exits 0 in
    /// silence. "The roll restored nothing" is the answer an operator needs
    /// most and the one an empty table hides.
    #[tokio::test]
    async fn a_muster_that_restored_nothing_says_so_on_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, daemon) = fake_client_on(&path).await;
        // `queue_reply_then_event` is the one `shep_client::testing` helper
        // that answers an arbitrary `Response` to an arbitrary request —
        // `reply_to_list`/`reply_to_describe` only ever arm `ListFlock`/
        // `Describe`, neither of which this call sends. It also writes an
        // event right behind the reply (real-daemon subscribe ordering,
        // `server.rs:357`), which this plain `client.request_with_deadline`
        // call never subscribes to read; the event lands unread on the wire
        // and is dropped along with the connection, so it changes nothing
        // observable here. No `reply_to` was added: this one already covers
        // the case.
        daemon.queue_reply_then_event(
            Response::Mustered(Vec::new()),
            BusEvent::Process {
                event: ProcessEventKind::Online,
                info: sample_info(),
                manually: true,
                at_ms: 0,
            },
        );
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            muster(&client, &mut streams).await
        };
        assert_eq!(code, ExitCode::Success, "an empty roll is not a failure");
        assert!(
            !err.is_empty(),
            "an empty muster must not be a silent success"
        );
    }
}
