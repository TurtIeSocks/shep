//! `bleats` (alias `logs`): following a sheep's log stream. The only
//! streaming verb, and the only one whose output skips
//! [`crate::output`]'s envelope: a follow has no end, so there is nothing
//! to wrap.
//!
//! One `Request::ListFlock` resolves an id -> [`ProcessInfo`] cache before
//! subscribing, since the daemon's topic filter carries no sheep identity:
//! selector filtering happens client-side against that cache.
//!
//! `--no-follow` never subscribes: it tails each matched sheep's log files
//! and exits. `--follow` prints that same tail first, then subscribes.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use futures_util::FutureExt;
use serde::Serialize;

use shep_client::{Client, EventStream, Lagged};
use shep_core::protocol::{BusEvent, ProcessInfo, Request, Response};
use shep_core::selector::ProcessSelector;

use crate::cli::{BleatsArgs, Format};
use crate::commands::selector::parse_selector;
use crate::exit::ExitCode;
use crate::output::{self, Streams, write_outcome};

/// One line of `bleats` output under `--format json`. A stability surface of
/// its own, not wrapped in [`output::OutputEnvelope`]: a follow has no end.
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
    /// The instance slot this line's sheep occupies, when its app has more
    /// than one instance registered; `null` when the app has only one, or
    /// when the line's origin cannot be attributed to a single instance (a
    /// backlog line read from a file several instances share).
    instance: Option<u32>,
    /// The line itself, no trailing newline.
    line: &'a str,
}

/// Issues the one `Request::ListFlock` `bleats` sends, before it ever
/// subscribes, and turns the answer into an id -> [`ProcessInfo`] cache.
///
/// # Errors
/// Renders and returns the exit code for a request that failed to reach
/// the daemon, or a response this client does not recognise (`Response`
/// is `#[non_exhaustive]`).
async fn resolve_names(
    client: &Client,
    streams: &mut Streams<'_>,
) -> Result<HashMap<u32, ProcessInfo>, ExitCode> {
    match client.request(Request::ListFlock).await {
        Ok(Response::Flock(procs)) => Ok(procs.into_iter().map(|p| (p.id, p)).collect()),
        Ok(_unrecognised) => {
            let message = "the daemon answered with a response this client does not understand";
            Err(streams.fail(ExitCode::Internal, message))
        }
        Err(err) => {
            let code = ExitCode::from(&err);
            Err(streams.fail(code, &err.to_string()))
        }
    }
}

/// Subscribes to every topic `bleats` needs: `log.*` for the lines
/// themselves, `daemon.*` so a `BusEvent::DaemonShutdown` is observed
/// rather than the connection simply vanishing unexplained.
///
/// # Errors
/// Renders and returns the exit code for a subscribe request that failed.
async fn subscribe(client: &Client, streams: &mut Streams<'_>) -> Result<EventStream, ExitCode> {
    let topics = vec!["log.*".to_string(), "daemon.*".to_string()];
    match client.subscribe(topics).await {
        Ok(stream) => Ok(stream),
        Err(err) => {
            let code = ExitCode::from(&err);
            Err(streams.fail(code, &err.to_string()))
        }
    }
}

/// Resolves `id` to a name from `cache`, or the bare id if `id` was not in
/// the one listing `resolve_names` took. An id that shows up on the bus
/// later than that snapshot is not a reason to block on a second listing.
fn resolved_name(cache: &HashMap<u32, ProcessInfo>, id: u32) -> String {
    cache
        .get(&id)
        .map_or_else(|| id.to_string(), |info| info.name.clone())
}

/// The slot a followed line's sheep occupies, or `None` when `id` was not
/// in the one listing `resolve_names` took, or its app has only one
/// instance.
///
/// The daemon emits [`BusEvent::LogOut`]/[`LogErr`](BusEvent::LogErr) per
/// sheep, so a follow labels a line even when several instances share one
/// log file, unlike the backlog path.
fn resolved_instance(cache: &HashMap<u32, ProcessInfo>, id: u32) -> Option<u32> {
    let info = cache.get(&id)?;
    if instance_count(cache, &info.name) > 1 {
        info.instance
    } else {
        None
    }
}

/// Whether `selector` (parsed client-side) admits `id`, matched against
/// `cache`'s snapshot of that sheep if it has one.
///
/// An id the initial listing never saw has no name or fold to match
/// against, so it is matched with an empty name and no fold: enough for
/// `all` and for `ProcessSelector::Id`, while a name, regex or fold
/// selector excludes it.
fn selector_allows(selector: &ProcessSelector, cache: &HashMap<u32, ProcessInfo>, id: u32) -> bool {
    match cache.get(&id) {
        Some(info) => selector.matches(&info.name, info.id, info.fold.as_deref(), info.instance),
        None => selector.matches("", id, None, None),
    }
}

/// Writes one rendered line to `out`. `stream` is `"out"` or `"err"`, the
/// sheep stream the line came from, not `out`'s own identity: every line
/// this function is called with lands on [`Streams::out`].
fn write_line(
    out: &mut dyn io::Write,
    fmt: Format,
    id: u32,
    name: &str,
    instance: Option<u32>,
    stream: &'static str,
    line: &str,
) -> io::Result<()> {
    match fmt {
        Format::Json => {
            let payload = BleatLine {
                schema_version: output::SCHEMA_VERSION,
                id,
                name,
                instance,
                stream,
                line,
            };
            serde_json::to_writer(&mut *out, &payload)?;
            writeln!(out)
        }
        Format::Table => match instance {
            Some(slot) => writeln!(out, "{name}:{slot} | {line}"),
            None => writeln!(out, "{name} | {line}"),
        },
    }
}

/// How many rows of `cache` carry `name`, counted over the whole cache and
/// never a selector's matched subset, so a selector cannot change how a line
/// is labelled: `shep bleats web:0` still prints `web:0`, not `web`.
fn instance_count(cache: &HashMap<u32, ProcessInfo>, name: &str) -> usize {
    cache.values().filter(|info| info.name == name).count()
}

/// One of `bleats`' own notices, unless `--quiet` asked for silence.
///
/// Goes out through [`output::emit_notice`], not [`output::emit_error`]:
/// its code is not part of [`crate::exit::ExitCode`]'s taxonomy, and a
/// clean run can emit one on its way to exit 0. The `quiet` gate is this
/// verb's own, not [`Streams::aside`]'s; a sheep's own line and a real
/// error both still print under it.
fn write_notice(streams: &mut Streams<'_>, quiet: bool, code: &str, message: &str) {
    if quiet {
        return;
    }
    streams.aside(code, message);
}

/// The most of one log file a tail will read to find the lines it wants.
///
/// Binds only when lines average over 5 KiB, so in ordinary use the caller's
/// line count is the bound that decides. A line count alone cannot bound
/// memory: one arbitrarily long line with no newline would defeat it.
const TAIL_WINDOW_BYTES: u64 = 256 * 1024;

/// The last `limit` lines of one log file, bounded twice: a
/// [`TAIL_WINDOW_BYTES`] window from the end of the file, then `limit`
/// lines within it. Returns the lines and whether either bound cut them
/// short.
///
/// `std::fs`, not `tokio::fs`: shep-cli's tokio has no `fs` feature. Each
/// line loses its daemon-added timestamp ([`shep_core::logstamp`]), so it
/// reads the same as a line from the bus. A non-zero seek discards bytes
/// up to the first `\n`, rather than rendering a mid-line fragment.
///
/// # Errors
/// The file could not be opened, `stat`ed, seeked, or read.
pub(crate) fn read_tail(path: &Path, limit: usize) -> io::Result<(Vec<String>, bool)> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(TAIL_WINDOW_BYTES);
    // `start > 0` means the byte window itself left content behind, before
    // a single line has been counted: the file is bigger than the window.
    let window_truncated = start > 0;
    if start > 0 {
        file.seek(SeekFrom::Start(start))?;
    }

    let mut window = Vec::new();
    file.read_to_end(&mut window)?;

    let window: &[u8] = if start > 0 {
        match window.iter().position(|&b| b == b'\n') {
            Some(newline) => &window[newline + 1..],
            None => &[],
        }
    } else {
        &window
    };

    let text = String::from_utf8_lossy(window);
    // The daemon's per-line stamp comes off here, so a `line` has one
    // meaning across both of this verb's paths: the follow path reads the
    // bus, which carries a sheep's own bytes. The stamp stays in the file
    // for `tail`, `less` and `grep`.
    let mut lines: Vec<String> = text
        .split('\n')
        .map(|line| shep_core::logstamp::strip(line).to_string())
        .collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let keep_from = lines.len().saturating_sub(limit);
    let truncated = window_truncated || keep_from > 0;
    lines.drain(..keep_from);
    Ok((lines, truncated))
}

/// Renders the selected files of every sheep the selector admits, in flock
/// order, and returns the exit code that reports how that went.
///
/// Within one sheep, `out_file` (unless `--err`) prints before `err_file`
/// (unless `--out`), with no merge between them. `cache` is a `HashMap`, so
/// matched sheep are sorted first, by name, then instance slot, then id.
///
/// A `None` path (a shepherd predating
/// [`shep_core::protocol::ProcessInfo::out_file`]) is a `log_path_unknown`
/// notice. A missing file is silent, since the daemon creates both at
/// spawn. Any other read failure is a `log_unreadable` notice and sets
/// [`ExitCode::Failure`]; the rest of the flock still prints.
fn tail_log_files(
    streams: &mut Streams<'_>,
    quiet: bool,
    cache: &HashMap<u32, ProcessInfo>,
    selector: &ProcessSelector,
    args: &BleatsArgs,
) -> ExitCode {
    let mut matched: Vec<&ProcessInfo> = cache
        .values()
        .filter(|info| selector.matches(&info.name, info.id, info.fold.as_deref(), info.instance))
        .collect();
    // `(name, instance, id)`, the key `shep_core::protocol::sort_flock`
    // takes, though not that helper: these are `&ProcessInfo` borrowed out of
    // the cache. Without the slot, a reloaded app's instances sort wrong, a
    // reload giving slot 0 a fresh high id.
    matched.sort_unstable_by(|a, b| {
        (a.name.as_str(), a.instance, a.id).cmp(&(b.name.as_str(), b.instance, b.id))
    });

    let mut failure = false;

    // One file, one read. Several instances can resolve to one path: every
    // `merge_logs` app does, and so does any app that set `out_file`
    // explicitly.
    let mut seen_paths: HashSet<String> = HashSet::new();
    let mut seen_notices: HashSet<String> = HashSet::new();

    // Whether a path is shared between several rows, over the whole cache
    // rather than the matched subset: a selector narrowing to one row must
    // not hide that the file is still shared.
    let mut path_owners: HashMap<(&'static str, String), usize> = HashMap::new();
    for info in cache.values() {
        for (stream_name, path) in [
            ("out", info.out_file.as_deref()),
            ("err", info.err_file.as_deref()),
        ] {
            if let Some(path) = path {
                *path_owners
                    .entry((stream_name, path.to_string()))
                    .or_insert(0) += 1;
            }
        }
    }

    for info in matched {
        let name = &info.name;
        let wanted: [(&'static str, Option<&str>, bool); 2] = [
            ("out", info.out_file.as_deref(), !args.err),
            ("err", info.err_file.as_deref(), !args.out),
        ];
        for (stream_name, path, show) in wanted {
            if !show {
                continue;
            }
            match path {
                None => {
                    // No path to key on, since the missing field is why this
                    // fires. The message already names the pair that varies.
                    let message =
                        format!("{name}: the daemon did not report a {stream_name} log path");
                    if seen_notices.insert(message.clone()) {
                        write_notice(streams, quiet, "log_path_unknown", &message);
                    }
                }
                Some(path) => {
                    if !seen_paths.insert(path.to_string()) {
                        continue;
                    }
                    // Only label a backlog line with a slot when this path
                    // belongs to exactly one row: instances sharing one file
                    // interleave in it and no line says who wrote it.
                    let shared = path_owners
                        .get(&(stream_name, path.to_string()))
                        .copied()
                        .unwrap_or(0)
                        > 1;
                    let label_instance = if !shared && instance_count(cache, name) > 1 {
                        info.instance
                    } else {
                        None
                    };
                    match read_tail(Path::new(path), args.lines) {
                        Ok((lines, _truncated)) => {
                            for line in lines {
                                if let Err(write_err) = write_line(
                                    streams.out,
                                    streams.fmt,
                                    info.id,
                                    name,
                                    label_instance,
                                    stream_name,
                                    &line,
                                ) {
                                    let code = write_outcome(Err(write_err));
                                    let _ = streams.out.flush();
                                    return code;
                                }
                            }
                        }
                        Err(err) if err.kind() == io::ErrorKind::NotFound => {
                            // Silent: the daemon creates both files at spawn,
                            // so a missing file means this sheep has never
                            // run in this $SHEP_HOME. A notice per quiet
                            // sheep would spam stderr on a fresh flock.
                        }
                        Err(err) => {
                            failure = true;
                            write_notice(
                                streams,
                                quiet,
                                "log_unreadable",
                                &format!("failed to read {path}: {err}"),
                            );
                        }
                    }
                }
            }
        }
    }

    let _ = streams.out.flush();
    if failure {
        ExitCode::Failure
    } else {
        ExitCode::Success
    }
}

/// Handles one [`BusEvent`] already known to be `Ok` (a `Lagged` item is
/// handled by the caller, not here).
///
/// `BusEvent` is `#[non_exhaustive]`: the `_` arm silently ignores anything
/// this client does not recognise, since a follow must not die on a bus event
/// a newer daemon added. `Dropped` is a named variant this client
/// understands, so it gets its own arm.
fn handle_event(
    streams: &mut Streams<'_>,
    quiet: bool,
    cache: &HashMap<u32, ProcessInfo>,
    selector: &ProcessSelector,
    args: &BleatsArgs,
    event: BusEvent,
) -> io::Result<()> {
    match event {
        BusEvent::LogOut { id, line } => {
            if !args.err && selector_allows(selector, cache, id) {
                let name = resolved_name(cache, id);
                let instance = resolved_instance(cache, id);
                write_line(streams.out, streams.fmt, id, &name, instance, "out", &line)?;
            }
            Ok(())
        }
        BusEvent::LogErr { id, line } => {
            if !args.out && selector_allows(selector, cache, id) {
                let name = resolved_name(cache, id);
                let instance = resolved_instance(cache, id);
                write_line(streams.out, streams.fmt, id, &name, instance, "err", &line)?;
            }
            Ok(())
        }
        BusEvent::Dropped { count } => {
            // Worded apart from the `Lagged` arm below: `Dropped` is the
            // daemon's own outbound queue overflowing for this subscriber,
            // `Lagged` is this client's receiver falling behind reading its
            // socket. Opposite ends of the connection to investigate.
            write_notice(
                streams,
                quiet,
                "dropped",
                &format!("the daemon dropped {count} events (its own queue overflowed)"),
            );
            Ok(())
        }
        BusEvent::DaemonShutdown => {
            // Shep's own diagnostic, not a sheep's line: `streams.err`.
            write_notice(
                streams,
                quiet,
                "daemon_shutdown",
                "the daemon is shutting down",
            );
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Follows the bleats (log output) of the sheep matching `args.selector`.
///
/// `quiet` is `cli::GlobalArgs::quiet`: it silences this module's own notices
/// and nothing else. A sheep's own line and a real error both still print.
///
/// Delegates to [`bleats_with_signal`] with a real `SIGINT` as the interrupt
/// source.
pub async fn bleats(
    client: &Client,
    streams: &mut Streams<'_>,
    quiet: bool,
    args: &BleatsArgs,
) -> ExitCode {
    bleats_with_signal(
        client,
        streams,
        quiet,
        args,
        tokio::signal::ctrl_c().map(|_| ()),
    )
    .await
}

/// [`bleats`] with the interrupt injected, so the Ctrl-C branch has a test
/// that does not need a real `SIGINT`.
///
/// One `Request::ListFlock` builds the id -> name cache both paths share.
/// `--no-follow` stops there and hands off to [`tail_log_files`], issuing
/// no `Request::Subscribe`. `--follow` subscribes and loops on
/// `tokio::select!`: a line renders, a `Lagged` item is noted and the
/// follow continues, the stream ending means the daemon is gone
/// ([`ExitCode::DaemonUnreachable`]), and `interrupt` firing exits
/// [`ExitCode::Success`].
///
/// `streams.out` is flushed on every exit path, or buffered lines are lost.
pub async fn bleats_with_signal(
    client: &Client,
    streams: &mut Streams<'_>,
    quiet: bool,
    args: &BleatsArgs,
    interrupt: impl std::future::Future<Output = ()> + Send,
) -> ExitCode {
    let selector = match parse_selector(streams, &args.selector) {
        Ok(selector) => selector,
        Err(code) => return code,
    };

    // The id/name cache is built from one listing taken before subscribing.
    // Subscribing first would lose every line pushed while the listing is
    // still in flight.
    let cache = match resolve_names(client, streams).await {
        Ok(cache) => cache,
        Err(code) => return code,
    };

    if args.no_follow {
        return tail_log_files(streams, quiet, &cache, &selector, args);
    }

    // Backlog before subscribing, so a line in the gap is missed rather
    // than printed twice. The tail's exit code is discarded: an unreadable
    // log for one sheep must not stop the follow over the whole flock.
    if args.lines > 0 {
        let _ = tail_log_files(streams, quiet, &cache, &selector, args);
    }

    let mut stream = match subscribe(client, streams).await {
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
                        if let Err(write_err) =
                            handle_event(streams, quiet, &cache, &selector, args, event)
                        {
                            let code = write_outcome(Err(write_err));
                            let _ = streams.out.flush();
                            return code;
                        }
                    }
                    Some(Err(Lagged { count })) => {
                        write_notice(
                            streams,
                            quiet,
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
        ProcessInfo::builder(id, name, ProcStatus::Online)
            .pid(Some(1000 + id))
            .out_file(Some(format!("/logs/{name}-0-out.log")))
            .err_file(Some(format!("/logs/{name}-0-err.log")))
            .build()
    }

    /// Like [`info`], but with a real instance slot: `info` never sets one.
    fn info_with_instance(id: u32, name: &str, slot: u32) -> ProcessInfo {
        ProcessInfo::builder(id, name, ProcStatus::Online)
            .pid(Some(1000 + id))
            .instance(Some(slot))
            .out_file(Some(format!("/logs/{name}-{slot}-out.log")))
            .err_file(Some(format!("/logs/{name}-{slot}-err.log")))
            .build()
    }

    fn bleats_args(selector: &str, no_follow: bool, err: bool, out: bool) -> BleatsArgs {
        BleatsArgs {
            selector: selector.to_string(),
            no_follow,
            // The follow tests here assert on what the bus delivers, so
            // this asks for no history.
            lines: crate::cli::DEFAULT_BLEAT_LINES,
            err,
            out,
        }
    }

    fn follow_args(selector: &str) -> BleatsArgs {
        BleatsArgs {
            lines: 0,
            ..bleats_args(selector, false, false, false)
        }
    }

    fn follow_args_err(selector: &str) -> BleatsArgs {
        bleats_args(selector, false, true, false)
    }

    fn follow_args_out(selector: &str) -> BleatsArgs {
        bleats_args(selector, false, false, true)
    }

    fn no_follow_args(selector: &str) -> BleatsArgs {
        bleats_args(selector, true, false, false)
    }

    fn no_follow_args_err(selector: &str) -> BleatsArgs {
        bleats_args(selector, true, true, false)
    }

    fn no_follow_args_out(selector: &str) -> BleatsArgs {
        bleats_args(selector, true, false, true)
    }

    /// Writes `content` to `dir/name` and returns the path as a `String`,
    /// what a scripted [`ProcessInfo`]'s `out_file`/`err_file` needs.
    fn write_log(dir: &Path, name: &str, content: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path.to_str().unwrap().to_string()
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

        // `--no-follow` is `ArgAction::SetTrue` and stores no value, so a
        // following token is not consumed by it and lands on the positional.
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
    /// bounded by this timeout, so a hang fails with a named assertion
    /// instead of a killed CI job.
    const RUN_TIMEOUT: Duration = Duration::from_secs(5);

    /// `daemon.close_after_subscribe()`, not `daemon.close()`: it ends the
    /// connection only after the real `Subscribe` this test's `bleats` call
    /// issues has been served and every `push`ed event flushed, so a follow
    /// running to end-of-stream observes everything in order regardless of
    /// scheduling.
    #[tokio::test]
    async fn ids_resolve_to_names_from_one_listing_and_unknown_ids_render_bare() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
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
        daemon.close_after_subscribe().await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, false, &follow_args("all")),
            )
            .await
            .expect("close_after_subscribe ends the follow deterministically, not by hanging");
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

    /// Same `close_after_subscribe` reasoning as the test above.
    #[tokio::test]
    async fn err_and_out_filter_the_two_streams() {
        for (args, kept, gone) in [
            (follow_args_err("all"), "to-stderr", "to-stdout"),
            (follow_args_out("all"), "to-stdout", "to-stderr"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = shep_client::testing::control_address(dir.path());
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
            daemon.close_after_subscribe().await;

            let mut out = Vec::new();
            let mut err = Vec::new();
            {
                let mut streams = Streams {
                    out: &mut out,
                    err: &mut err,
                    style: crate::style::Presentation::BARE,
                    fmt: Format::Table,
                };
                tokio::time::timeout(RUN_TIMEOUT, bleats(&client, &mut streams, false, &args))
                    .await
                    .expect(
                        "close_after_subscribe ends the follow deterministically, not by hanging",
                    );
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

    /// The daemon's topic filter globs on `log.out` / `log.err`, which
    /// carry no sheep identity, so this filtering must happen client-side.
    #[tokio::test]
    async fn a_selector_filters_client_side_on_the_resolved_id_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
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
        daemon.close_after_subscribe().await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, false, &follow_args("web")),
            )
            .await
            .expect("close_after_subscribe ends the follow deterministically, not by hanging");
        }
        let out = String::from_utf8(out).unwrap();

        assert!(out.contains("from-web"));
        assert!(
            !out.contains("from-worker"),
            "the selector must narrow the resolved id set: {out}"
        );
    }

    /// A follow always knows which sheep wrote a line, the daemon emitting
    /// `BusEvent::LogOut` per sheep, so it labels a multi-instance app's
    /// lines with their slot even though the two rows here would share a
    /// file on the backlog path.
    #[tokio::test]
    async fn a_multi_instance_apps_follow_labels_its_lines_with_the_slot() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, daemon) = fake_client_with_push(&path).await;
        daemon.reply_to_list(vec![
            info_with_instance(1, "web", 0),
            info_with_instance(2, "web", 1),
        ]);
        daemon
            .push(BusEvent::LogOut {
                id: 1,
                line: "from-slot-0".into(),
            })
            .await;
        daemon
            .push(BusEvent::LogOut {
                id: 2,
                line: "from-slot-1".into(),
            })
            .await;
        daemon.close_after_subscribe().await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, false, &follow_args("web")),
            )
            .await
            .expect("close_after_subscribe ends the follow deterministically, not by hanging");
        }
        let out = String::from_utf8(out).unwrap();

        assert!(
            out.contains("web:0 | from-slot-0"),
            "a followed line must carry its slot: {out}"
        );
        assert!(
            out.contains("web:1 | from-slot-1"),
            "a followed line must carry its slot: {out}"
        );
    }

    /// A selector narrowed to one instance must not change how it is
    /// labelled: `instance_count` counts over the whole cache, so `web:0`
    /// still prints `web:0` even though the cache holds a `web:1` this
    /// selector excludes.
    #[tokio::test]
    async fn a_selector_narrowed_to_one_instance_does_not_change_its_label() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, daemon) = fake_client_with_push(&path).await;
        daemon.reply_to_list(vec![
            info_with_instance(1, "web", 0),
            info_with_instance(2, "web", 1),
        ]);
        daemon
            .push(BusEvent::LogOut {
                id: 1,
                line: "from-slot-0".into(),
            })
            .await;
        daemon.close_after_subscribe().await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, false, &follow_args("web:0")),
            )
            .await
            .expect("close_after_subscribe ends the follow deterministically, not by hanging");
        }
        let out = String::from_utf8(out).unwrap();

        assert!(
            out.contains("web:0 | from-slot-0"),
            "a selector narrowed to one instance must not strip its label: {out}"
        );
    }

    /// Guards `resolved_instance`'s "more than one instance registered"
    /// check: this row carries `.instance(Some(0))`, so returning it
    /// unconditionally would still print `web:0` here.
    #[tokio::test]
    async fn a_single_instance_app_with_a_slot_on_its_row_still_follows_bare() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, daemon) = fake_client_with_push(&path).await;
        daemon.reply_to_list(vec![info_with_instance(1, "web", 0)]);
        daemon
            .push(BusEvent::LogOut {
                id: 1,
                line: "hello".into(),
            })
            .await;
        daemon.close_after_subscribe().await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, false, &follow_args("web")),
            )
            .await
            .expect("close_after_subscribe ends the follow deterministically, not by hanging");
        }
        let out = String::from_utf8(out).unwrap();

        assert!(
            out.contains("web | hello"),
            "one registered instance means no slot to report, even though this row \
             carries one: {out}"
        );
        assert!(
            !out.contains("web:0"),
            "a slot must not leak onto a single-instance app's followed output: {out}"
        );
    }

    /// Guards `handle_event`'s `LogErr` arm specifically: it threads
    /// `instance` on its own, separately from `LogOut`'s. A follow that
    /// labelled stdout but not stderr passes every other test in this
    /// module and fails only this one.
    #[tokio::test]
    async fn a_multi_instance_apps_followed_stderr_line_carries_its_slot_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, daemon) = fake_client_with_push(&path).await;
        daemon.reply_to_list(vec![
            info_with_instance(1, "web", 0),
            info_with_instance(2, "web", 1),
        ]);
        daemon
            .push(BusEvent::LogErr {
                id: 2,
                line: "boom".into(),
            })
            .await;
        daemon.close_after_subscribe().await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, false, &follow_args("web")),
            )
            .await
            .expect("close_after_subscribe ends the follow deterministically, not by hanging");
        }
        let out = String::from_utf8(out).unwrap();

        assert!(
            out.contains("web:1 | boom"),
            "a followed stderr line must carry its slot exactly as stdout does: {out}"
        );
    }

    /// The stream stays open for the whole test, so only the injected
    /// interrupt can end this follow; ignoring it hangs and the timeout
    /// fails the test.
    #[tokio::test]
    async fn ctrl_c_during_a_follow_exits_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
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
            style: crate::style::Presentation::BARE,
            fmt: Format::Table,
        };

        let args = follow_args("all");
        let follow = bleats_with_signal(&client, &mut streams, false, &args, async {
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

    /// Both end in `DaemonUnreachable`, so the exit code alone discriminates
    /// nothing; the notice text is the behaviour under test.
    ///
    /// `close_after_subscribe`, not `close()`: this test needs the
    /// connection to end mid-follow, deterministically, right after
    /// `Subscribe` is served.
    #[tokio::test]
    async fn a_daemon_shutdown_mid_follow_is_announced_before_the_stream_ends() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
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
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, false, &follow_args("all")),
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

    #[tokio::test]
    async fn a_stream_that_just_ends_reports_no_shutdown_notice() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, daemon) = fake_client_with_push(&path).await;
        daemon.reply_to_list(vec![info(1, "web")]);
        daemon.close_after_subscribe().await; // no DaemonShutdown event at all

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, false, &follow_args("all")),
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

    /// Same shutdown scenario as
    /// [`a_daemon_shutdown_mid_follow_is_announced_before_the_stream_ends`],
    /// with `quiet: true`: the exit code must not move, only the notice
    /// text.
    #[tokio::test]
    async fn quiet_suppresses_the_daemon_shutdown_notice_but_not_the_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, daemon) = fake_client_with_push(&path).await;
        daemon.reply_to_list(vec![info(1, "web")]);
        daemon.push(BusEvent::DaemonShutdown).await;
        daemon.close_after_subscribe().await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, true, &follow_args("all")),
            )
            .await
            .expect("a shutdown mid-follow must end the follow, not hang")
        };

        assert_eq!(
            code,
            ExitCode::DaemonUnreachable,
            "quiet must not change the exit code, only whether the notice prints"
        );
        assert!(
            String::from_utf8(err).unwrap().is_empty(),
            "quiet must suppress the shutdown notice entirely"
        );
    }

    /// `resolved_name`/`write_line` never see `quiet` (their call sites are
    /// unconditional in `handle_event`), so this checks end-to-end that a
    /// sheep's own line still reaches `streams.out` under `quiet: true`.
    #[tokio::test]
    async fn quiet_does_not_suppress_a_sheeps_own_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, daemon) = fake_client_with_push(&path).await;
        daemon.reply_to_list(vec![info(1, "web")]);
        daemon
            .push(BusEvent::LogOut {
                id: 1,
                line: "hello".into(),
            })
            .await;
        daemon.close_after_subscribe().await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, true, &follow_args("all")),
            )
            .await
            .expect("close_after_subscribe ends the follow deterministically, not by hanging");
        }
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("web") && out.contains("hello"),
            "quiet must never touch a sheep's own line: {out}"
        );
    }

    /// Depends on the default current-thread runtime: `overrun_by` pushes
    /// `EVENT_CHANNEL_CAPACITY + n` events in one burst, and only a
    /// receiver that has not yet been scheduled falls behind enough to see
    /// a `Lagged`. Under `multi_thread`, real parallelism keeps the
    /// receiver caught up and no lag is produced.
    ///
    /// `cfg(unix)`: a named pipe wakes its reader on a different schedule,
    /// so on Windows the receiver keeps pace and the lag never triggers,
    /// the same way `multi_thread` defeats it above.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_lag_notice_reaches_stderr_and_the_follow_continues() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, daemon) = fake_client_with_push(&path).await;
        daemon.reply_to_list(vec![info(1, "web")]);
        daemon.overrun_by(8).await; // forces a Lagged item
        daemon
            .push(BusEvent::LogOut {
                id: 1,
                line: "after".into(),
            })
            .await;
        daemon.close_after_subscribe().await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, false, &follow_args("all")),
            )
            .await
            .expect("close_after_subscribe ends the follow deterministically, not by hanging");
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

    /// Fails if `BusEvent::Dropped` falls into `handle_event`'s `_ =>
    /// Ok(())` catch-all and vanishes.
    ///
    /// `Dropped` (the daemon's queue) and `Lagged` (this client's receiver
    /// falling behind) must read differently, so this asserts the
    /// daemon-side wording specifically: a bare `stderr.contains("dropped")`
    /// would also pass if the `Lagged` arm's wording were reused by mistake.
    #[tokio::test]
    async fn a_dropped_notice_reaches_stderr_worded_for_the_daemon_side_cause() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, daemon) = fake_client_with_push(&path).await;
        daemon.reply_to_list(vec![info(1, "web")]);
        daemon.push(BusEvent::Dropped { count: 5 }).await;
        daemon.close_after_subscribe().await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, false, &follow_args("all")),
            )
            .await
            .expect("close_after_subscribe ends the follow deterministically, not by hanging");
        }

        let stderr = String::from_utf8(err).unwrap();
        assert!(
            stderr.contains("daemon") && stderr.contains('5'),
            "a daemon-side Dropped must not be silently swallowed: {stderr}"
        );
        assert!(
            !stderr.contains("locally"),
            "Dropped is the daemon's queue overflowing, not this client \
             falling behind reading its own socket — reusing the `Lagged` \
             arm's wording would blame the wrong side: {stderr}"
        );
    }

    /// Every other test in this module uses `Format::Table`, so a JSON
    /// line shape change (a renamed field, or rendering table rows under
    /// `--format json`) would leave every other test green.
    #[tokio::test]
    async fn json_format_renders_the_pinned_six_key_line_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, daemon) = fake_client_with_push(&path).await;
        daemon.reply_to_list(vec![info(1, "web")]);
        daemon
            .push(BusEvent::LogErr {
                id: 1,
                line: "boom".into(),
            })
            .await;
        daemon.close_after_subscribe().await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Json,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, false, &follow_args("all")),
            )
            .await
            .expect("close_after_subscribe ends the follow deterministically, not by hanging");
        }
        let out = String::from_utf8(out).unwrap();
        let line = out.lines().next().expect("one JSON line was rendered");
        let json: serde_json::Value = serde_json::from_str(line).unwrap();
        let obj = json.as_object().expect("a bleats JSON line is an object");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["id", "instance", "line", "name", "schema_version", "stream"],
            "the bleats JSON line shape is a stability surface: {out}"
        );
        assert_eq!(json["stream"], "err", "the stream this line came from");
        assert_eq!(
            json["instance"],
            serde_json::Value::Null,
            "one registered instance means no slot to report"
        );
    }

    /// A writer that always fails with `BrokenPipe`: `shep bleats | head`
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

    /// `write_outcome` already treats a `BrokenPipe` write as
    /// [`ExitCode::Success`]; this exercises that path through an actual
    /// write failure.
    ///
    /// The write fails on the very first event, well before
    /// `close_after_subscribe` could end the stream.
    #[tokio::test]
    async fn a_broken_pipe_while_writing_a_line_exits_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, daemon) = fake_client_with_push(&path).await;
        daemon.reply_to_list(vec![info(1, "web")]);
        daemon
            .push(BusEvent::LogOut {
                id: 1,
                line: "hello".into(),
            })
            .await;
        daemon.close_after_subscribe().await;

        let mut out = BrokenPipeWriter;
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, false, &follow_args("all")),
            )
            .await
            .expect("close_after_subscribe ends the follow deterministically, not by hanging")
        };

        assert_eq!(
            code,
            ExitCode::Success,
            "a reader closing the pipe is not a failed command"
        );
    }

    // --- `--no-follow` reads the log files ---
    // None of these subscribe, so there is nothing for `RUN_TIMEOUT` to
    // guard but a bounded file read.

    /// A `--no-follow` still wired to the bus fails the second assertion; one
    /// wired to neither fails the first. This is the test that tells the two
    /// apart from a `--no-follow` that reads the right files.
    #[tokio::test]
    async fn no_follow_reads_the_files_and_never_the_bus() {
        let dir = tempfile::tempdir().unwrap();
        let sock = shep_client::testing::control_address(dir.path());
        let out_path = write_log(dir.path(), "web-out.log", "from-the-file\n");

        let (client, daemon) = fake_client_with_push(&sock).await;
        let mut sheep = info(1, "web");
        sheep.out_file = Some(out_path);
        daemon.reply_to_list(vec![sheep]);
        daemon
            .push(BusEvent::LogOut {
                id: 1,
                line: "from-the-bus".into(),
            })
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
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, false, &no_follow_args_out("all")),
            )
            .await
            .expect("--no-follow never subscribes, so it must terminate on its own")
        };
        let rendered = String::from_utf8(out).unwrap();

        assert_eq!(code, ExitCode::Success);
        assert!(rendered.contains("from-the-file"));
        assert!(
            !rendered.contains("from-the-bus"),
            "the file path must never consult the bus: {rendered}"
        );
    }

    /// A sheep that already crashed before `shep bleats <name>` runs: the
    /// backlog is what makes its last output reachable without starting it
    /// again in a second window.
    #[tokio::test]
    async fn following_prints_the_existing_log_before_it_follows() {
        let dir = tempfile::tempdir().unwrap();
        let sock = shep_client::testing::control_address(dir.path());
        let out_path = write_log(
            dir.path(),
            "web-out.log",
            "boot: reading config\nFATAL: port 19999 already in use\n",
        );

        let (client, daemon) = fake_client_with_push(&sock).await;
        let mut sheep = info(1, "web");
        sheep.out_file = Some(out_path);
        daemon.reply_to_list(vec![sheep]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            let _ = tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, false, &follow_args_out("all")),
            )
            .await;
        }
        let rendered = String::from_utf8(out).unwrap();
        assert!(
            rendered.contains("FATAL: port 19999 already in use"),
            "a follow must carry the reason a dead sheep died: {rendered}"
        );
    }

    /// `--lines 0` is the escape hatch for someone who genuinely wants only
    /// what arrives next, and it is what the foreground runner passes.
    #[tokio::test]
    async fn lines_zero_follows_without_replaying_anything() {
        let dir = tempfile::tempdir().unwrap();
        let sock = shep_client::testing::control_address(dir.path());
        let out_path = write_log(dir.path(), "web-out.log", "OLD-HISTORY\n");

        let (client, daemon) = fake_client_with_push(&sock).await;
        let mut sheep = info(1, "web");
        sheep.out_file = Some(out_path);
        daemon.reply_to_list(vec![sheep]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            let _ = tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(
                    &client,
                    &mut streams,
                    false,
                    &BleatsArgs {
                        lines: 0,
                        ..follow_args_out("all")
                    },
                ),
            )
            .await;
        }
        let rendered = String::from_utf8(out).unwrap();
        assert!(
            !rendered.contains("OLD-HISTORY"),
            "--lines 0 must replay nothing: {rendered}"
        );
    }

    /// A `read_to_string`-style implementation prints line 1 and fails this.
    #[tokio::test]
    async fn the_tail_is_bounded_by_lines() {
        let dir = tempfile::tempdir().unwrap();
        let sock = shep_client::testing::control_address(dir.path());
        const CAP: usize = 50;
        let total = CAP + 20;
        let content: String = (1..=total).map(|n| format!("line-{n}\n")).collect();
        let out_path = write_log(dir.path(), "web-out.log", &content);

        let (client, daemon) = fake_client_with_push(&sock).await;
        let mut sheep = info(1, "web");
        sheep.out_file = Some(out_path);
        daemon.reply_to_list(vec![sheep]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(
                    &client,
                    &mut streams,
                    false,
                    &BleatsArgs {
                        lines: CAP,
                        ..no_follow_args_out("all")
                    },
                ),
            )
            .await
            .expect("--no-follow never subscribes, so it must terminate on its own");
        }
        let rendered = String::from_utf8(out).unwrap();

        assert!(
            !rendered.lines().any(|line| line == "web | line-1"),
            "the first line must fall outside the tail: {rendered}"
        );
        assert!(
            rendered
                .lines()
                .any(|line| line == format!("web | line-{total}")),
            "the last line must be present: {rendered}"
        );
        assert_eq!(
            rendered.lines().count(),
            CAP,
            "exactly CAP lines must reach stdout: {rendered}"
        );
    }

    /// Guards the window and the discard-the-partial-first-line rule
    /// together: an implementation that keeps the partial head emits a
    /// quarter-megabyte fragment and fails this.
    #[tokio::test]
    async fn the_tail_is_bounded_by_bytes_and_never_shows_half_a_line() {
        let dir = tempfile::tempdir().unwrap();
        let sock = shep_client::testing::control_address(dir.path());
        let long_line = "x".repeat(usize::try_from(TAIL_WINDOW_BYTES).unwrap() + 1024);
        let content = format!("{long_line}\nshort\n");
        let out_path = write_log(dir.path(), "web-out.log", &content);

        let (client, daemon) = fake_client_with_push(&sock).await;
        let mut sheep = info(1, "web");
        sheep.out_file = Some(out_path);
        daemon.reply_to_list(vec![sheep]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, false, &no_follow_args_out("all")),
            )
            .await
            .expect("--no-follow never subscribes, so it must terminate on its own");
        }
        let rendered = String::from_utf8(out).unwrap();

        assert_eq!(
            rendered,
            "web | short\n",
            "no fragment of the long line may reach stdout ({} bytes rendered)",
            rendered.len()
        );
    }

    /// Two instances sharing one log file (a `merge_logs` app, or any app
    /// with an explicit `out_file`) must be read once, not once per
    /// instance: reading per row printed the whole file once per instance
    /// pointing at it.
    #[test]
    fn instances_sharing_one_log_file_are_read_once_not_once_each() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shared = dir.path().join("talker-out.log");
        std::fs::write(&shared, "line one\nline two\n").expect("write");

        let shared_path = shared.to_string_lossy().to_string();
        let mut cache = HashMap::new();
        for id in 0..2u32 {
            cache.insert(
                id,
                ProcessInfo::builder(id, "talker", ProcStatus::Online)
                    .out_file(Some(shared_path.clone()))
                    .err_file(Some(shared_path.clone()))
                    .build(),
            );
        }

        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
            style: crate::style::Presentation::BARE,
            fmt: Format::Table,
        };
        let args = no_follow_args_out("talker");
        let selector = ProcessSelector::parse("talker").expect("selector");

        tail_log_files(&mut streams, false, &cache, &selector, &args);

        let printed = String::from_utf8(out).expect("utf8");
        assert_eq!(
            printed.matches("line one").count(),
            1,
            "one file, one read, however many instances point at it:\n{printed}"
        );
    }

    /// Builds a cache of `count` rows for one app, and returns it with the
    /// printed backlog. `shared` puts every instance on one file, the way
    /// `merge_logs` does.
    fn backlog_of(dir: &Path, app: &str, count: u32, shared: bool) -> String {
        let mut cache = HashMap::new();
        for slot in 0..count {
            let stem = if shared {
                format!("{app}-out.log")
            } else {
                format!("{app}-{slot}-out.log")
            };
            let path = dir.join(&stem);
            std::fs::write(&path, format!("hello from {slot}\n")).expect("write");
            cache.insert(
                slot,
                ProcessInfo::builder(slot, app, ProcStatus::Online)
                    .instance(Some(slot))
                    .out_file(Some(path.to_string_lossy().to_string()))
                    .build(),
            );
        }
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
            style: crate::style::Presentation::BARE,
            fmt: Format::Table,
        };
        let args = no_follow_args_out(app);
        let selector = ProcessSelector::parse(app).expect("selector");
        tail_log_files(&mut streams, false, &cache, &selector, &args);
        String::from_utf8(out).expect("utf8")
    }

    #[test]
    fn a_multi_instance_app_labels_its_backlog_lines_with_the_slot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let printed = backlog_of(dir.path(), "web", 2, false);
        assert!(printed.contains("web:0 |"), "{printed}");
        assert!(printed.contains("web:1 |"), "{printed}");
    }

    #[test]
    fn a_single_instance_app_keeps_the_bare_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let printed = backlog_of(dir.path(), "solo", 1, false);
        assert!(printed.contains("solo |"), "{printed}");
        assert!(
            !printed.contains("solo:0"),
            "no suffix for one instance: {printed}"
        );
    }

    #[test]
    fn a_shared_backlog_file_is_labelled_with_the_app_not_a_slot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let printed = backlog_of(dir.path(), "talker", 2, true);
        assert!(printed.contains("talker |"), "{printed}");
        assert!(
            !printed.contains("talker:"),
            "one file holds both instances, and no line says which wrote it: {printed}"
        );
    }

    /// `truncated` must report `true` when the byte window alone cut the
    /// tail short: a sheep logging a few long, structured lines can fill
    /// `TAIL_WINDOW_BYTES` in far fewer than `limit` lines.
    #[test]
    fn read_tail_reports_truncated_on_a_byte_window_cut_alone() {
        let dir = tempfile::tempdir().unwrap();
        let long_line = "x".repeat(usize::try_from(TAIL_WINDOW_BYTES).unwrap() + 1024);
        let content = format!("{long_line}\nshort\n");
        let path = dir.path().join("web-out.log");
        std::fs::write(&path, &content).unwrap();

        let (lines, truncated) = read_tail(&path, 50).unwrap();

        assert_eq!(lines, vec!["short".to_string()]);
        assert!(
            truncated,
            "the file exceeds the byte window, so this tail is not the whole log"
        );
    }

    /// Three ways: `--out` (out lines only), `--err` (err lines only),
    /// neither (out lines, then err lines, this module's own within-sheep
    /// ordering).
    #[tokio::test]
    async fn out_and_err_select_which_file_is_read() {
        async fn run(args: BleatsArgs) -> String {
            let dir = tempfile::tempdir().unwrap();
            let sock = shep_client::testing::control_address(dir.path());
            let out_path = write_log(dir.path(), "web-out.log", "stdout-line\n");
            let err_path = write_log(dir.path(), "web-err.log", "stderr-line\n");

            let (client, daemon) = fake_client_with_push(&sock).await;
            let mut sheep = info(1, "web");
            sheep.out_file = Some(out_path);
            sheep.err_file = Some(err_path);
            daemon.reply_to_list(vec![sheep]);

            let mut out = Vec::new();
            let mut err = Vec::new();
            {
                let mut streams = Streams {
                    out: &mut out,
                    err: &mut err,
                    style: crate::style::Presentation::BARE,
                    fmt: Format::Table,
                };
                tokio::time::timeout(RUN_TIMEOUT, bleats(&client, &mut streams, false, &args))
                    .await
                    .expect("--no-follow never subscribes, so it must terminate on its own");
            }
            String::from_utf8(out).unwrap()
        }

        let out_only = run(no_follow_args_out("all")).await;
        assert!(out_only.contains("stdout-line") && !out_only.contains("stderr-line"));

        let err_only = run(no_follow_args_err("all")).await;
        assert!(err_only.contains("stderr-line") && !err_only.contains("stdout-line"));

        let both = run(no_follow_args("all")).await;
        let out_pos = both
            .find("stdout-line")
            .expect("the stdout line is present");
        let err_pos = both
            .find("stderr-line")
            .expect("the stderr line is present");
        assert!(
            out_pos < err_pos,
            "out_file must render before err_file within one sheep: {both}"
        );
    }

    /// Fails if the sheep are tailed in id order rather than name order.
    ///
    /// `b` holds the lower id but sorts after `a` by name, and the listing
    /// is scripted in id order, so neither a `HashMap`'s arbitrary order
    /// nor id order can make the assertion pass by accident.
    #[tokio::test]
    async fn files_are_printed_in_name_order() {
        let dir = tempfile::tempdir().unwrap();
        let sock = shep_client::testing::control_address(dir.path());
        let a_path = write_log(dir.path(), "a-out.log", "line-from-a\n");
        let b_path = write_log(dir.path(), "b-out.log", "line-from-b\n");

        let (client, daemon) = fake_client_with_push(&sock).await;
        // `b` takes the LOWER id, so id order and name order disagree.
        let mut sheep_b = info(1, "b");
        sheep_b.out_file = Some(b_path);
        let mut sheep_a = info(2, "a");
        sheep_a.out_file = Some(a_path);
        daemon.reply_to_list(vec![sheep_b, sheep_a]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, false, &no_follow_args_out("all")),
            )
            .await
            .expect("--no-follow never subscribes, so it must terminate on its own");
        }
        let rendered = String::from_utf8(out).unwrap();

        let a_pos = rendered.find("line-from-a").expect("a's line is present");
        let b_pos = rendered.find("line-from-b").expect("b's line is present");
        assert!(
            a_pos < b_pos,
            "name order puts `a` (id 2) before `b` (id 1): {rendered}"
        );
    }

    /// Fails if one app's instances are tailed in id order rather than
    /// slot order: a reload gives slot 0 a fresh high id, so id order
    /// alone would read 1, 2, 0.
    ///
    /// Slot 0 is given the highest id here and the listing is scripted in
    /// slot order, so neither id order nor a `HashMap`'s arbitrary order
    /// can make the assertion pass by accident.
    #[tokio::test]
    async fn one_apps_instances_are_printed_in_slot_order() {
        let dir = tempfile::tempdir().unwrap();
        let sock = shep_client::testing::control_address(dir.path());

        let (client, daemon) = fake_client_with_push(&sock).await;
        let mut listing = Vec::new();
        // Slot 0 reloaded, so it holds id 9 while slots 1 and 2 kept 1 and 2.
        for (id, slot) in [(9_u32, 0_u32), (1, 1), (2, 2)] {
            let mut sheep = info(id, "web");
            sheep.instance = Some(slot);
            sheep.out_file = Some(write_log(
                dir.path(),
                &format!("web-{slot}-out.log"),
                &format!("line-from-slot-{slot}\n"),
            ));
            listing.push(sheep);
        }
        daemon.reply_to_list(listing);

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, false, &no_follow_args_out("all")),
            )
            .await
            .expect("--no-follow never subscribes, so it must terminate on its own");
        }
        let rendered = String::from_utf8(out).unwrap();

        let at = |slot: u32| {
            rendered
                .find(&format!("line-from-slot-{slot}"))
                .unwrap_or_else(|| panic!("slot {slot}'s line is present: {rendered}"))
        };
        assert!(
            at(0) < at(1) && at(1) < at(2),
            "slot order, not id order (which would read 1, 2, 0): {rendered}"
        );
    }

    /// The daemon creates both files at spawn, so a missing one means this
    /// sheep has never run in this `$SHEP_HOME`, not a fault worth a
    /// notice.
    #[tokio::test]
    async fn a_missing_file_is_silent_and_the_rest_still_print() {
        let dir = tempfile::tempdir().unwrap();
        let sock = shep_client::testing::control_address(dir.path());
        let real_path = write_log(dir.path(), "web-out.log", "still-here\n");
        let missing_path = dir
            .path()
            .join("never-written.log")
            .to_str()
            .unwrap()
            .to_string();

        let (client, daemon) = fake_client_with_push(&sock).await;
        let mut ghost = info(1, "ghost");
        ghost.out_file = Some(missing_path);
        let mut real = info(2, "web");
        real.out_file = Some(real_path);
        daemon.reply_to_list(vec![ghost, real]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, false, &no_follow_args_out("all")),
            )
            .await
            .expect("--no-follow never subscribes, so it must terminate on its own")
        };

        assert_eq!(code, ExitCode::Success);
        assert!(String::from_utf8(out).unwrap().contains("still-here"));
        assert!(
            err.is_empty(),
            "a missing file is silent, not a notice: {}",
            String::from_utf8_lossy(&err)
        );
    }

    /// Points `out_file` at a directory, not a `chmod 000` file: opening a
    /// directory succeeds on unix and the read fails `EISDIR`
    /// deterministically, including as root, where a `000` file would still
    /// be readable.
    #[tokio::test]
    async fn an_unreadable_file_is_noticed_and_exits_failure_with_the_rest_still_printed() {
        let dir = tempfile::tempdir().unwrap();
        let sock = shep_client::testing::control_address(dir.path());
        let bad_dir = dir.path().join("a-directory");
        std::fs::create_dir(&bad_dir).unwrap();
        let bad_dir = bad_dir.to_str().unwrap().to_string();
        let real_path = write_log(dir.path(), "web-out.log", "still-here\n");

        let (client, daemon) = fake_client_with_push(&sock).await;
        let mut bad = info(1, "bad");
        bad.out_file = Some(bad_dir.clone());
        let mut real = info(2, "web");
        real.out_file = Some(real_path);
        daemon.reply_to_list(vec![bad, real]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, false, &no_follow_args_out("all")),
            )
            .await
            .expect("--no-follow never subscribes, so it must terminate on its own")
        };

        assert_eq!(code, ExitCode::Failure);
        assert!(
            String::from_utf8(out).unwrap().contains("still-here"),
            "one sheep's unreadable file must not hide the rest of the flock's lines"
        );
        let stderr = String::from_utf8(err).unwrap();
        assert!(
            stderr.contains(&bad_dir),
            "the notice must name the unreadable path: {stderr}"
        );
    }

    /// An implementation that skips a `None` path in silence passes every
    /// other test here and fails this one.
    #[tokio::test]
    async fn a_daemon_that_reported_no_path_is_noticed_not_silently_empty() {
        let dir = tempfile::tempdir().unwrap();
        let sock = shep_client::testing::control_address(dir.path());
        let (client, daemon) = fake_client_with_push(&sock).await;
        let mut sheep = info(1, "web");
        sheep.out_file = None;
        sheep.err_file = None;
        daemon.reply_to_list(vec![sheep]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, false, &no_follow_args_out("all")),
            )
            .await
            .expect("--no-follow never subscribes, so it must terminate on its own")
        };

        assert_eq!(
            code,
            ExitCode::Success,
            "version skew is not a fault in this run"
        );
        let stderr = String::from_utf8(err).unwrap();
        assert!(
            stderr.contains("log_path_unknown"),
            "a None path must be noticed, not silently empty: {stderr}"
        );
    }

    /// Sits beside `json_format_renders_the_pinned_six_key_line_shape`:
    /// renaming a field of `BleatLine` must now fail both.
    #[tokio::test]
    async fn a_file_sourced_json_line_is_the_same_six_key_shape_as_a_bus_sourced_one() {
        let dir = tempfile::tempdir().unwrap();
        let sock = shep_client::testing::control_address(dir.path());
        let out_path = write_log(dir.path(), "web-out.log", "hello-from-disk\n");

        let (client, daemon) = fake_client_with_push(&sock).await;
        let mut sheep = info(1, "web");
        sheep.out_file = Some(out_path);
        daemon.reply_to_list(vec![sheep]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Json,
            };
            tokio::time::timeout(
                RUN_TIMEOUT,
                bleats(&client, &mut streams, false, &no_follow_args_out("all")),
            )
            .await
            .expect("--no-follow never subscribes, so it must terminate on its own");
        }
        let out = String::from_utf8(out).unwrap();
        let line = out.lines().next().expect("one JSON line was rendered");
        let json: serde_json::Value = serde_json::from_str(line).unwrap();
        let obj = json.as_object().expect("a bleats JSON line is an object");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();

        assert_eq!(
            keys,
            ["id", "instance", "line", "name", "schema_version", "stream"],
            "a file-sourced line must be the same shape as a bus-sourced one: {out}"
        );
        assert_eq!(json["id"], 1);
        assert_eq!(json["name"], "web");
        assert_eq!(json["stream"], "out");
        assert_eq!(
            json["instance"],
            serde_json::Value::Null,
            "one registered instance means no slot to report"
        );
        assert_eq!(json["line"], "hello-from-disk");
        assert_eq!(json["schema_version"], output::SCHEMA_VERSION);
    }
}
