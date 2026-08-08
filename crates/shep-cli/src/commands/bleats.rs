//! `bleats` (alias `logs`): following a sheep's log stream — the only
//! streaming verb, and the only one whose output does not go through
//! [`crate::output`]'s envelope (see that module's own doc: a follow has
//! no end, so there is nothing to wrap).
//!
//! Order matters here in a way it does not for the other verbs: one
//! `Request::ListFlock` resolves an id -> [`ProcessInfo`] cache *before*
//! `Request::Subscribe` goes out, never the other way around — subscribing
//! first would lose every line the daemon pushes while the listing is
//! still in flight. An id that later shows up on the bus but was not in
//! that one listing renders as its bare id rather than blocking on a
//! second `ListFlock` per unknown line.
//!
//! Selector filtering happens **client-side**, against the id set that one
//! listing resolved: the daemon's own topic filter globs on the topic
//! string (`log.out`, `log.err`), which carries no sheep identity at all,
//! so the daemon has nothing to narrow by selector with — only this
//! module can.
//!
//! `--out`/`--err` choose which of a sheep's two output streams is shown,
//! not where shep's own text goes: both a followed sheep's stdout and its
//! stderr are the data the user asked for, so both land on
//! [`Streams::out`]. [`Streams::err`] carries only shep's own
//! diagnostics — the lag notice, the shutdown notice — never a line a
//! sheep itself wrote; interleaving sheep stderr into it would make
//! `shep bleats > file` silently lose half the output, and `--err` would
//! produce an empty file.

use std::collections::HashMap;
use std::io;

use futures_util::{FutureExt, StreamExt};
use serde::Serialize;

use shep_client::{Client, EventStream, Lagged};
use shep_core::protocol::{BusEvent, ProcessInfo, Request, Response};
use shep_core::selector::ProcessSelector;

use crate::cli::{BleatsArgs, Format};
use crate::exit::ExitCode;
use crate::output::{self, Streams, write_outcome};

/// One line of `bleats` output under `--format json` — a stability surface
/// of its own (Task 12's fixture), deliberately not wrapped in
/// [`output::OutputEnvelope`]: see this module's own doc for why.
#[derive(Debug, Serialize)]
struct BleatLine<'a> {
    /// [`output::SCHEMA_VERSION`] at the time this line was produced.
    schema_version: u32,
    /// The sheep's id.
    id: u32,
    /// The sheep's name if the initial listing resolved it, else the bare
    /// id rendered as a string.
    name: &'a str,
    /// Which of a sheep's two output streams this line came from.
    stream: &'static str,
    /// The line itself, no trailing newline.
    line: &'a str,
}

/// Parses `raw` client-side, so a malformed selector is a fast local usage
/// error rather than a round trip to the daemon.
///
/// Returns a [`ProcessSelector`] rather than a `SelectorSpec`: unlike every
/// other selector-taking verb, `bleats` never puts the selector on the
/// wire at all (the daemon's topic filter has no sheep identity to match
/// one against) — it only ever matches locally, against the id/name cache
/// [`resolve_names`] builds.
fn parse_selector(
    streams: &mut Streams<'_>,
    fmt: Format,
    raw: &str,
) -> Result<ProcessSelector, ExitCode> {
    match ProcessSelector::parse(raw) {
        Ok(selector) => Ok(selector),
        Err(err) => {
            let _ = output::emit_error(
                &mut *streams.err,
                fmt,
                ExitCode::Usage.code_str(),
                &err.to_string(),
            );
            Err(ExitCode::Usage)
        }
    }
}

/// Issues the one `Request::ListFlock` `bleats` sends, before it ever
/// subscribes, and turns the answer into an id -> [`ProcessInfo`] cache.
///
/// # Errors
/// Renders and returns the exit code for a request that failed to reach
/// the daemon, or a response this client does not recognise (`Response` is
/// `#[non_exhaustive]`, Global Constraints).
async fn resolve_names(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
) -> Result<HashMap<u32, ProcessInfo>, ExitCode> {
    match client.request(Request::ListFlock).await {
        Ok(Response::Flock(procs)) => Ok(procs.into_iter().map(|p| (p.id, p)).collect()),
        Ok(_unrecognised) => {
            let message = "the daemon answered with a response this client does not understand";
            let _ = output::emit_error(
                &mut *streams.err,
                fmt,
                ExitCode::Internal.code_str(),
                message,
            );
            Err(ExitCode::Internal)
        }
        Err(err) => {
            let code = ExitCode::from(&err);
            let _ = output::emit_error(&mut *streams.err, fmt, code.code_str(), &err.to_string());
            Err(code)
        }
    }
}

/// Subscribes to every topic `bleats` needs: `log.*` for the lines
/// themselves, `daemon.*` so a `BusEvent::DaemonShutdown` is observed
/// rather than the connection simply vanishing unexplained.
///
/// # Errors
/// Renders and returns the exit code for a subscribe request that failed.
async fn subscribe(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
) -> Result<EventStream, ExitCode> {
    let topics = vec!["log.*".to_string(), "daemon.*".to_string()];
    match client.subscribe(topics).await {
        Ok(stream) => Ok(stream),
        Err(err) => {
            let code = ExitCode::from(&err);
            let _ = output::emit_error(&mut *streams.err, fmt, code.code_str(), &err.to_string());
            Err(code)
        }
    }
}

/// Resolves `id` to a name from `cache`, or the bare id if `id` was not in
/// the one listing `resolve_names` took — an id that shows up on the bus
/// later than that snapshot is not a reason to block on a second listing.
fn resolved_name(cache: &HashMap<u32, ProcessInfo>, id: u32) -> String {
    cache
        .get(&id)
        .map_or_else(|| id.to_string(), |info| info.name.clone())
}

/// Whether `selector` (parsed client-side) admits `id`, matched against
/// `cache`'s snapshot of that sheep if it has one.
///
/// An id the initial listing never saw has no name or fold to match
/// against; it is matched with an empty name and no fold, which is enough
/// for `all` and for `ProcessSelector::Id` (both work without identity)
/// while a name/regex/fold selector correctly excludes it — there is
/// nothing to prove it belongs.
fn selector_allows(selector: &ProcessSelector, cache: &HashMap<u32, ProcessInfo>, id: u32) -> bool {
    match cache.get(&id) {
        Some(info) => selector.matches(&info.name, info.id, info.fold.as_deref()),
        None => selector.matches("", id, None),
    }
}

/// Writes one rendered line to `out`. `stream` is `"out"` or `"err"` — the
/// sheep stream the line came from, not `out`'s own identity: every line
/// this function is called with lands on [`Streams::out`], per this
/// module's own doc.
fn write_line(
    out: &mut dyn io::Write,
    fmt: Format,
    id: u32,
    name: &str,
    stream: &'static str,
    line: &str,
) -> io::Result<()> {
    match fmt {
        Format::Json => {
            let payload = BleatLine {
                schema_version: output::SCHEMA_VERSION,
                id,
                name,
                stream,
                line,
            };
            serde_json::to_writer(&mut *out, &payload)?;
            writeln!(out)
        }
        Format::Table => writeln!(out, "{name} | {line}"),
    }
}

/// Writes one of this module's own notices — not a sheep's line, and not
/// `parse_selector`'s kind of usage error either — to `streams.err`, in the
/// same grammar [`output::emit_error`] gives usage errors.
///
/// Without this, a script capturing stderr under `--format json` would see
/// valid JSON from `parse_selector`'s errors alongside plain-text prose from
/// everything else this module writes: two grammars for one command's
/// stderr, item 7's fix.
fn write_notice(streams: &mut Streams<'_>, fmt: Format, code: &str, message: &str) {
    let _ = output::emit_error(&mut *streams.err, fmt, code, message);
}

/// Handles one [`BusEvent`] already known to be `Ok` (a `Lagged` item is
/// handled by the caller, not here).
///
/// `BusEvent` is `#[non_exhaustive]`: the `_` arm ignores anything this
/// client does not recognise, silently — a follow must not die on a bus
/// event a newer daemon added (Global Constraints). `Dropped` is NOT one of
/// those unrecognised events — it is a real, named variant this client
/// understands — so it gets its own arm rather than falling into that `_`.
fn handle_event(
    streams: &mut Streams<'_>,
    fmt: Format,
    cache: &HashMap<u32, ProcessInfo>,
    selector: &ProcessSelector,
    args: &BleatsArgs,
    event: BusEvent,
) -> io::Result<()> {
    match event {
        BusEvent::LogOut { id, line } => {
            if !args.err && selector_allows(selector, cache, id) {
                let name = resolved_name(cache, id);
                write_line(streams.out, fmt, id, &name, "out", &line)?;
            }
            Ok(())
        }
        BusEvent::LogErr { id, line } => {
            if !args.out && selector_allows(selector, cache, id) {
                let name = resolved_name(cache, id);
                write_line(streams.out, fmt, id, &name, "err", &line)?;
            }
            Ok(())
        }
        BusEvent::Dropped { count } => {
            // Daemon-side cause, deliberately NOT the `Lagged` arm's
            // wording below: `Dropped` is the daemon's own outbound queue
            // overflowing for this subscriber, while `Lagged` is this
            // client's receiver falling behind reading its socket. The two
            // failures live on opposite sides of the connection and must
            // read differently, or a user cannot tell which end to
            // investigate.
            write_notice(
                streams,
                fmt,
                "dropped",
                &format!("the shepherd dropped {count} events (its own queue overflowed)"),
            );
            Ok(())
        }
        BusEvent::DaemonShutdown => {
            // Shep's own diagnostic, not a sheep's line: `streams.err`.
            write_notice(
                streams,
                fmt,
                "daemon_shutdown",
                "the shepherd is shutting down",
            );
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Follows the bleats (log output) of the sheep matching `args.selector`.
///
/// Delegates to [`bleats_with_signal`] with a real `SIGINT` as the
/// interrupt source — see that function's own doc for the shape both
/// share.
pub async fn bleats(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    args: &BleatsArgs,
) -> ExitCode {
    bleats_with_signal(
        client,
        streams,
        fmt,
        args,
        tokio::signal::ctrl_c().map(|_| ()),
    )
    .await
}

/// [`bleats`] with the interrupt injected, so the Ctrl-C branch has a test
/// that does not need a real `SIGINT` — one would kill the test runner.
///
/// One `Request::ListFlock`, then one `Request::Subscribe`
/// (`log.*`/`daemon.*`), then a loop over one `tokio::select!` with three
/// arms, checked in this priority order every iteration:
///
/// 1. The event stream — a normal line is rendered, a `Lagged` item is
///    noted to `streams.err` and the follow continues, and the stream
///    ending (`None`) means the daemon is gone: flush and exit
///    [`ExitCode::DaemonUnreachable`].
/// 2. `interrupt` — a user ending a follow deliberately has not failed:
///    flush and exit [`ExitCode::Success`].
/// 3. Only under `--no-follow` (`args.no_follow`): resolves immediately
///    once arm 1 has nothing ready *right now*, which is what "drain what
///    is buffered and exit instead of streaming" means in practice —
///    flush and exit [`ExitCode::Success`]. Absent under `--follow`
///    (the default): with no third arm ready, the loop simply waits on
///    the other two, which is exactly a follow's job.
///
/// `streams.out` is flushed on every exit path — a follow that ends with
/// lines still buffered would otherwise lose them silently.
pub async fn bleats_with_signal(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    args: &BleatsArgs,
    interrupt: impl std::future::Future<Output = ()> + Send,
) -> ExitCode {
    let follow = !args.no_follow;

    let selector = match parse_selector(streams, fmt, &args.selector) {
        Ok(selector) => selector,
        Err(code) => return code,
    };

    // Order matters: the id/name cache is built from ONE listing taken
    // before subscribing. Subscribing first would lose every line pushed
    // while the listing is still in flight.
    let cache = match resolve_names(client, streams, fmt).await {
        Ok(cache) => cache,
        Err(code) => return code,
    };

    let mut stream = match subscribe(client, streams, fmt).await {
        Ok(stream) => stream,
        Err(code) => return code,
    };

    tokio::pin!(interrupt);

    loop {
        tokio::select! {
            biased;
            item = stream.next() => {
                match item {
                    Some(Ok(event)) => {
                        if let Err(write_err) = handle_event(streams, fmt, &cache, &selector, args, event) {
                            let code = write_outcome(Err(write_err));
                            let _ = streams.out.flush();
                            return code;
                        }
                    }
                    Some(Err(Lagged { count })) => {
                        write_notice(
                            streams,
                            fmt,
                            "lagged",
                            &format!("{count} events dropped locally (lagged)"),
                        );
                    }
                    None => {
                        let _ = streams.out.flush();
                        return ExitCode::DaemonUnreachable;
                    }
                }
            }
            () = &mut interrupt => {
                let _ = streams.out.flush();
                return ExitCode::Success;
            }
            () = std::future::ready(()), if !follow => {
                let _ = streams.out.flush();
                return ExitCode::Success;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use shep_client::testing::fake_client_with_push;
    use shep_core::protocol::ProcessInfo;
    use shep_core::status::ProcStatus;

    use super::*;
    use crate::cli::{Cli, Commands};

    fn info(id: u32, name: &str) -> ProcessInfo {
        ProcessInfo {
            id,
            name: name.to_string(),
            status: ProcStatus::Online,
            pid: Some(1000 + id),
            restarts: 0,
            uptime_ms: 0,
            fold: None,
            out_file: Some(format!("/logs/{name}-0-out.log")),
            err_file: Some(format!("/logs/{name}-0-err.log")),
        }
    }

    fn bleats_args(selector: &str, no_follow: bool, err: bool, out: bool) -> BleatsArgs {
        BleatsArgs {
            selector: selector.to_string(),
            no_follow,
            err,
            out,
        }
    }

    fn follow_args(selector: &str) -> BleatsArgs {
        bleats_args(selector, false, false, false)
    }

    fn drain_args(selector: &str) -> BleatsArgs {
        bleats_args(selector, true, false, false)
    }

    fn drain_args_err(selector: &str) -> BleatsArgs {
        bleats_args(selector, true, true, false)
    }

    fn drain_args_out(selector: &str) -> BleatsArgs {
        bleats_args(selector, true, false, true)
    }

    #[test]
    fn no_follow_parses_and_plain_bleats_still_follows() {
        use clap::Parser;

        let Commands::Bleats(args) = Cli::try_parse_from(["shep", "bleats"]).unwrap().command
        else {
            panic!()
        };
        assert!(!args.no_follow, "the default is to follow");

        let Commands::Bleats(args) = Cli::try_parse_from(["shep", "bleats", "--no-follow"])
            .unwrap()
            .command
        else {
            panic!()
        };
        assert!(args.no_follow);

        // The flag stores NO value: `--no-follow` is `ArgAction::SetTrue`, so a
        // following token is not consumed by it and lands on the positional
        // instead.
        let Commands::Bleats(args) = Cli::try_parse_from(["shep", "bleats", "--no-follow", "true"])
            .unwrap()
            .command
        else {
            panic!()
        };
        assert!(args.no_follow);
        assert_eq!(
            args.selector, "true",
            "--no-follow takes no value; the token is the selector"
        );
    }

    /// Every `bleats(...)`/`bleats_with_signal(...)` call in this module is
    /// bounded by this timeout — a broken implementation that hangs (e.g. a
    /// drain that never terminates, or a follow that never observes an
    /// interrupt) fails with a named assertion instead of a killed CI job
    /// (Global Constraints: nine tests have already shipped that fail only
    /// by hanging).
    const RUN_TIMEOUT: Duration = Duration::from_secs(5);

    /// Deliberate deviation from the brief: `daemon.close()` is NOT called
    /// here. Empirically (a throwaway diagnostic test against the existing,
    /// already-merged `FakeDaemon`), calling `close()` before the client
    /// under test has issued even one real request kills the connection
    /// outright — `FakeDaemon::close` consumes `self` and joins the
    /// background task, and by the time it returns the actor has already
    /// observed EOF and will fail every future request with
    /// `RequestError::Closed`. `bleats`'s mandatory first step is a real
    /// `Request::ListFlock`, so a `close()` here would make every assertion
    /// below unreachable. `--no-follow` drain mode does not need the
    /// connection to end anyway — it terminates on its own once nothing
    /// more is immediately available — so the fix is simply not calling it.
    ///
    /// **Known limitation, not a solved problem**: this test — and every
    /// other `--no-follow` drain-mode test in this module — is green only
    /// under the default current-thread test runtime. None of them
    /// synchronize on "the pushed events actually reached the client"
    /// before the drain arm (`std::future::ready(())`, gated `if !follow`)
    /// resolves; they rely on the current-thread scheduler having already
    /// driven the actor's socket read into the broadcast channel by the
    /// time the test task resumes. Switching to
    /// `#[tokio::test(flavor = "multi_thread")]` fails every one of these
    /// tests nondeterministically (verified 15/15 runs). Adding
    /// `close_after_subscribe()` does NOT fix it: the drain arm can still
    /// win the `select!` race before the actor has forwarded anything, since
    /// closing the connection is not the same event as delivering a queued
    /// item. The fake this module relies on has no sync point a real fix
    /// would need — a future flavor switch here is not a mystery, but it is
    /// also not free.
    #[tokio::test]
    async fn ids_resolve_to_names_from_one_listing_and_unknown_ids_render_bare() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, daemon) = fake_client_with_push(&path).await;
        daemon.reply_to_list(vec![info(1, "web")]);
        daemon
            .push(BusEvent::LogOut {
                id: 1,
                line: "hello".into(),
            })
            .await;
        daemon
            .push(BusEvent::LogOut {
                id: 9,
                line: "orphan".into(),
            })
            .await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, Format::Table, &drain_args("all")),
            )
            .await
            .expect("drain mode must terminate on its own, not hang");
        }
        let out = String::from_utf8(out).unwrap();

        assert!(out.contains("web") && out.contains("hello"));
        assert!(
            out.contains('9') && out.contains("orphan"),
            "an unknown id renders bare, not blocked on: {out}"
        );
        assert_eq!(
            daemon.list_flock_count(),
            1,
            "one listing, not one per unknown line"
        );
    }

    /// Same deviation as above, and for the same reason: drain mode does
    /// not need `daemon.close()` to terminate. Same current-thread-scheduler
    /// dependence as `ids_resolve_to_names_from_one_listing_and_unknown_ids_render_bare`'s
    /// doc describes — known limitation, not fixed here.
    #[tokio::test]
    async fn err_and_out_filter_the_two_streams() {
        for (args, kept, gone) in [
            (drain_args_err("all"), "to-stderr", "to-stdout"),
            (drain_args_out("all"), "to-stdout", "to-stderr"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("s.sock");
            let (client, daemon) = fake_client_with_push(&path).await;
            daemon.reply_to_list(vec![info(1, "web")]);
            daemon
                .push(BusEvent::LogOut {
                    id: 1,
                    line: "to-stdout".into(),
                })
                .await;
            daemon
                .push(BusEvent::LogErr {
                    id: 1,
                    line: "to-stderr".into(),
                })
                .await;

            let mut out = Vec::new();
            let mut err = Vec::new();
            {
                let mut streams = Streams {
                    out: &mut out,
                    err: &mut err,
                };
                tokio::time::timeout(
                    RUN_TIMEOUT,
                    bleats(&client, &mut streams, Format::Table, &args),
                )
                .await
                .expect("drain mode must terminate on its own, not hang");
            }
            let rendered = String::from_utf8(out).unwrap();
            assert!(
                rendered.contains(kept),
                "{kept} should have survived: {rendered}"
            );
            assert!(
                !rendered.contains(gone),
                "{gone} should have been filtered: {rendered}"
            );
        }
    }

    /// The daemon's topic filter globs on `log.out` / `log.err`, which carry
    /// no identity — so this filtering CANNOT have happened server-side,
    /// and a test that let the fake daemon pre-filter would prove nothing.
    /// Same `daemon.close()` deviation as the tests above, and the same
    /// current-thread-scheduler dependence documented on
    /// `ids_resolve_to_names_from_one_listing_and_unknown_ids_render_bare`.
    #[tokio::test]
    async fn a_selector_filters_client_side_on_the_resolved_id_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, daemon) = fake_client_with_push(&path).await;
        daemon.reply_to_list(vec![info(1, "web"), info(2, "worker")]);
        // The fake queues BOTH; only the selector may narrow them.
        daemon
            .push(BusEvent::LogOut {
                id: 1,
                line: "from-web".into(),
            })
            .await;
        daemon
            .push(BusEvent::LogOut {
                id: 2,
                line: "from-worker".into(),
            })
            .await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, Format::Table, &drain_args("web")),
            )
            .await
            .expect("drain mode must terminate on its own, not hang");
        }
        let out = String::from_utf8(out).unwrap();

        assert!(out.contains("from-web"));
        assert!(
            !out.contains("from-worker"),
            "the selector must narrow the resolved id set: {out}"
        );
    }

    /// The stream stays open for the whole test — the fake is never closed
    /// — so the ONLY thing that can end this follow is the injected
    /// interrupt. A `bleats` that ignored the interrupt arm hangs and the
    /// timeout fails it.
    #[tokio::test]
    async fn ctrl_c_during_a_follow_exits_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, daemon) = fake_client_with_push(&path).await;
        daemon.reply_to_list(vec![info(1, "web")]);
        daemon
            .push(BusEvent::LogOut {
                id: 1,
                line: "still running".into(),
            })
            .await;

        let (interrupt_tx, interrupt_rx) = tokio::sync::oneshot::channel::<()>();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };

        let args = follow_args("all");
        let follow = bleats_with_signal(&client, &mut streams, Format::Table, &args, async {
            let _ = interrupt_rx.await;
        });
        let (_, code) = tokio::join!(
            async {
                tokio::task::yield_now().await;
                let _ = interrupt_tx.send(()); // a oneshot stays ready once sent
            },
            tokio::time::timeout(RUN_TIMEOUT, follow),
        );
        assert_eq!(
            code.expect("the interrupt arm must end the follow"),
            ExitCode::Success,
            "a user ending a follow deliberately has not failed"
        );
    }

    /// The pair that makes the shutdown branch bite. Both end in
    /// `DaemonUnreachable` — the daemon went away either way — so the exit
    /// code alone discriminates nothing. The NOTICE is the behaviour under
    /// test: a `bleats` that never matches `BusEvent::DaemonShutdown` and
    /// just maps any end-of-stream to `DaemonUnreachable` passes the first
    /// assertion of each and fails the stderr assertion of the first.
    ///
    /// Deviation from the brief: `daemon.close_after_subscribe()` in place
    /// of `daemon.close()` — see the doc on
    /// `ids_resolve_to_names_from_one_listing_and_unknown_ids_render_bare`
    /// for why a plain `close()` before `bleats` even connects cannot work.
    /// This test genuinely needs the connection to end mid-follow (there is
    /// no interrupt here, and `follow_args` never terminates on its own),
    /// so unlike the drain tests above it cannot simply drop the close —
    /// `close_after_subscribe` ends it deterministically, right after the
    /// real `Subscribe` this test's `bleats` call issues has been served.
    #[tokio::test]
    async fn a_daemon_shutdown_mid_follow_is_announced_before_the_stream_ends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, daemon) = fake_client_with_push(&path).await;
        daemon.reply_to_list(vec![info(1, "web")]);
        daemon.push(BusEvent::DaemonShutdown).await; // scripted: emitted after Subscribe
        daemon.close_after_subscribe().await; // scripted: after Subscribe is served

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, Format::Table, &follow_args("all")),
            )
            .await
            .expect("a shutdown mid-follow must end the follow, not hang")
        };

        assert_eq!(code, ExitCode::DaemonUnreachable);
        assert!(
            String::from_utf8(err).unwrap().contains("shutting down"),
            "the shutdown notice is what distinguishes this from the connection simply ending"
        );
    }

    /// Same `close_after_subscribe` deviation as the test above, and for
    /// the same reason.
    #[tokio::test]
    async fn a_stream_that_just_ends_reports_no_shutdown_notice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, daemon) = fake_client_with_push(&path).await;
        daemon.reply_to_list(vec![info(1, "web")]);
        daemon.close_after_subscribe().await; // no DaemonShutdown event at all

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, Format::Table, &follow_args("all")),
            )
            .await
            .expect("the connection ending must end the follow, not hang")
        };

        assert_eq!(code, ExitCode::DaemonUnreachable);
        assert!(
            !String::from_utf8(err).unwrap().contains("shutting down"),
            "a notice the daemon never sent must not be invented"
        );
    }

    /// Same `daemon.close()` deviation as the drain tests above, and for
    /// the same reason: drain mode does not need the connection to end.
    /// Same current-thread-scheduler dependence documented on
    /// `ids_resolve_to_names_from_one_listing_and_unknown_ids_render_bare`.
    #[tokio::test]
    async fn a_lag_notice_reaches_stderr_and_the_follow_continues() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, daemon) = fake_client_with_push(&path).await;
        daemon.reply_to_list(vec![info(1, "web")]);
        daemon.overrun_by(8).await; // forces a Lagged item
        daemon
            .push(BusEvent::LogOut {
                id: 1,
                line: "after".into(),
            })
            .await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, Format::Table, &drain_args("all")),
            )
            .await
            .expect("drain mode must terminate on its own, not hang");
        }

        let stderr = String::from_utf8(err).unwrap();
        assert!(
            stderr.contains("dropped") || stderr.contains("lagged"),
            "a lag must be told, not swallowed: {stderr}"
        );
        assert!(
            String::from_utf8(out).unwrap().contains("after"),
            "a lag ends the gap, not the follow"
        );
    }

    /// Critical fix (item 1): `BusEvent::Dropped` used to fall into
    /// `handle_event`'s `_ => Ok(())` catch-all and vanish — the daemon's
    /// own outbound queue overflowing is exactly the "a sheep went quiet"
    /// failure mode this module's doc warns against swallowing silently.
    ///
    /// `Dropped` (the daemon's queue) and `Lagged` (this client's own
    /// receiver falling behind) are different causes and must read
    /// differently, so this asserts the daemon-side wording specifically —
    /// `stderr.contains("dropped")` alone would also pass if the `Lagged`
    /// arm's wording were reused by mistake, which is exactly the bug this
    /// test exists to catch.
    ///
    /// Same current-thread-scheduler dependence documented on
    /// `ids_resolve_to_names_from_one_listing_and_unknown_ids_render_bare`.
    #[tokio::test]
    async fn a_dropped_notice_reaches_stderr_worded_for_the_daemon_side_cause() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, daemon) = fake_client_with_push(&path).await;
        daemon.reply_to_list(vec![info(1, "web")]);
        daemon.push(BusEvent::Dropped { count: 5 }).await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, Format::Table, &drain_args("all")),
            )
            .await
            .expect("drain mode must terminate on its own, not hang");
        }

        let stderr = String::from_utf8(err).unwrap();
        assert!(
            stderr.contains("shepherd") && stderr.contains('5'),
            "a daemon-side Dropped must not be silently swallowed: {stderr}"
        );
        assert!(
            !stderr.contains("locally"),
            "Dropped is the daemon's queue overflowing, not this client \
             falling behind reading its own socket — reusing the `Lagged` \
             arm's wording would blame the wrong side: {stderr}"
        );
    }

    /// Important fix (item 3): every other test in this module uses
    /// `Format::Table`, so mutating the JSON line shape (renaming a field,
    /// or rendering table rows under `--format json`) left every test green.
    /// Global Constraints pins every command's JSON shape; this is that pin
    /// for `bleats`' own line shape (deferred by the brief to a Task 12
    /// fixture, pinned independently here since item 2 puts that fixture's
    /// shape in doubt).
    ///
    /// Same current-thread-scheduler dependence documented on
    /// `ids_resolve_to_names_from_one_listing_and_unknown_ids_render_bare`.
    #[tokio::test]
    async fn json_format_renders_the_pinned_five_key_line_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, daemon) = fake_client_with_push(&path).await;
        daemon.reply_to_list(vec![info(1, "web")]);
        daemon
            .push(BusEvent::LogErr {
                id: 1,
                line: "boom".into(),
            })
            .await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, Format::Json, &drain_args("all")),
            )
            .await
            .expect("drain mode must terminate on its own, not hang");
        }
        let out = String::from_utf8(out).unwrap();
        let line = out.lines().next().expect("one JSON line was rendered");
        let json: serde_json::Value = serde_json::from_str(line).unwrap();
        let obj = json.as_object().expect("a bleats JSON line is an object");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["id", "line", "name", "schema_version", "stream"],
            "the bleats JSON line shape is a stability surface: {out}"
        );
        assert_eq!(json["stream"], "err", "the stream this line came from");
    }

    /// A writer that always fails with `BrokenPipe` — `shep bleats | head`
    /// closing the reading end is the normal way this streaming verb ends,
    /// not an error.
    struct BrokenPipeWriter;

    impl io::Write for BrokenPipeWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Important fix (item 5): `shep bleats | head` is this verb's normal
    /// case, not an error — `write_outcome` already treats a `BrokenPipe`
    /// write failure as [`ExitCode::Success`], but nothing in this module
    /// exercised that path through an actual write failure. Same current-
    /// thread-scheduler dependence documented on
    /// `ids_resolve_to_names_from_one_listing_and_unknown_ids_render_bare`.
    #[tokio::test]
    async fn a_broken_pipe_while_writing_a_line_exits_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, daemon) = fake_client_with_push(&path).await;
        daemon.reply_to_list(vec![info(1, "web")]);
        daemon
            .push(BusEvent::LogOut {
                id: 1,
                line: "hello".into(),
            })
            .await;

        let mut out = BrokenPipeWriter;
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, Format::Table, &drain_args("all")),
            )
            .await
            .expect("drain mode must terminate on its own, not hang")
        };

        assert_eq!(
            code,
            ExitCode::Success,
            "a reader closing the pipe is not a failed command"
        );
    }
}
