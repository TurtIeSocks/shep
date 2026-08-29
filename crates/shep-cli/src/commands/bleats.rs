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
//!
//! `--no-follow` does not touch the bus at all: it takes the same one
//! `Request::ListFlock` [`resolve_names`] already sends, then prints the
//! tail of each matched sheep's log file and exits — `tail` to `--follow`'s
//! `tail -f`. It never subscribes, so there is no stream, no `Lagged`, no
//! shutdown notice, and no extra round trip. A file's tail is bounded
//! twice (`--lines` lines, found within the last [`TAIL_WINDOW_BYTES`]
//! of the file), and files are read one at a time, so peak memory for this
//! path is one window regardless of flock size.
//!
//! **Following prints that same tail first, then subscribes.** A sheep that
//! already crashed has said everything it is going to say, so a follow that
//! showed only new lines showed an empty screen while the reason sat in the
//! file — which is exactly how a boot-looping sheep came to look like it had
//! logged nothing at all.
//!
//! **The ordering limitation is real and is stated, not hidden.** Within one
//! file, lines print in file order (append order, chronological). Across a
//! sheep's two files there is no merge: `out_file` prints in full, then
//! `err_file` starts. A log line carries no timestamp, so there is no key to
//! interleave the two files on, and guessing one from arrival order would be
//! wrong exactly when a sheep writes to both streams at once — seeing all of
//! `out` before any of `err` must not be read as "everything on stdout
//! happened first". `--out`/`--err` sidestep the seam by reducing a sheep to
//! the one file that matters. `--follow` has no such limitation: the bus
//! delivers in arrival order, which is chronological across both streams.
//!
//! No-follow can also show a **stopped** sheep's last output, which
//! `--follow` cannot: the daemon creates both files at spawn and keeps
//! appending to them for the life of the sheep, so a sheep that has since
//! stopped still has a file to read, while it has nothing left to publish
//! to the bus.

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
/// the daemon, or a response this client does not recognise (`Response` is
/// `#[non_exhaustive]`, Global Constraints).
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
/// the one listing `resolve_names` took — an id that shows up on the bus
/// later than that snapshot is not a reason to block on a second listing.
fn resolved_name(cache: &HashMap<u32, ProcessInfo>, id: u32) -> String {
    cache
        .get(&id)
        .map_or_else(|| id.to_string(), |info| info.name.clone())
}

/// The slot a followed line's sheep occupies, or `None` when `id` was not
/// in the one listing `resolve_names` took, or when its app has only one
/// instance registered.
///
/// A follow always knows which sheep wrote a line -- the daemon emits
/// [`BusEvent::LogOut`]/[`LogErr`](BusEvent::LogErr) per sheep -- so unlike
/// the backlog path this labels a line even when several instances share
/// one log file (module doc, D11).
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
/// against; it is matched with an empty name and no fold, which is enough
/// for `all` and for `ProcessSelector::Id` (both work without identity)
/// while a name/regex/fold selector correctly excludes it — there is
/// nothing to prove it belongs.
fn selector_allows(selector: &ProcessSelector, cache: &HashMap<u32, ProcessInfo>, id: u32) -> bool {
    match cache.get(&id) {
        Some(info) => selector.matches(&info.name, info.id, info.fold.as_deref(), info.instance),
        None => selector.matches("", id, None, None),
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

/// How many rows of `cache` carry `name` -- counted over the WHOLE cache,
/// never a selector's matched subset, so a selector cannot change how a
/// line is labelled (`shep bleats web:0` still prints `web:0`, not `web`).
fn instance_count(cache: &HashMap<u32, ProcessInfo>, name: &str) -> usize {
    cache.values().filter(|info| info.name == name).count()
}

/// Writes one of this module's own notices — not a sheep's line, and not
/// `parse_selector`'s kind of usage error either — to `streams.err`, through
/// [`output::emit_notice`] rather than [`output::emit_error`]: a notice's
/// code (`log_path_unknown`, `dropped`, `daemon_shutdown`, ...) is not part
/// of [`crate::exit::ExitCode`]'s taxonomy, and a clean run can still emit
/// one on its way to exit 0 — reusing the error envelope would leave a
/// `--format json` consumer unable to tell a diagnostic from a failure
/// (whole-branch review item 4; `cli_e2e.rs`'s `assert_json_error` pins the
/// opposite rule for real errors: JSON on stderr means the command failed).
///
/// A no-op when `quiet` is set: `--quiet`'s own doc
/// (`cli::GlobalArgs::quiet`, "suppress non-essential output") is exactly
/// what a notice is — a sheep's own line and a real error both still print
/// regardless (whole-branch review item 2).
/// One of `bleats`' own notices, unless `--quiet` asked for silence.
///
/// Kept as a wrapper rather than folded into [`Streams::aside`] because the
/// `quiet` gate is this verb's, not every verb's: `bleats` is the one command
/// an operator leaves running, so its own asides are the ones worth being
/// able to switch off without losing a sheep's output.
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
/// memory on its own — one arbitrarily long line with no newline would
/// defeat it — hence both bounds.
const TAIL_WINDOW_BYTES: u64 = 256 * 1024;

/// The last `limit` lines of one log file, bounded twice over: a
/// [`TAIL_WINDOW_BYTES`] window from the end of the file, then `limit` once
/// that window is split into lines.
///
/// Returns the lines and whether EITHER bound was what cut them short — the
/// caller needs to tell "this is all of it" from "this is not", and
/// `whistle`'s `tail_bleats` (`crate::whistle::read`) surfaces that to a
/// model as `BleatTail::truncated`. A model that cannot tell the two apart
/// concludes a busy app went quiet. The line cap and the byte window are
/// both real ways to lose the older half of a log — a sheep logging long
/// structured lines fills [`TAIL_WINDOW_BYTES`] in under `limit` lines — so
/// a caller that only asked the line cap would report `false` on a tail
/// that is, in fact, not the whole story.
///
/// `limit` is a parameter rather than a constant because the callers each
/// have their own answer: [`tail_log_files`] passes `--lines`, and `whistle`
/// passes its own clamped `lines`.
///
/// `std::fs`, not `tokio::fs`: shep-cli's tokio does not carry the `fs`
/// feature, and this is a bounded read on a one-shot command with nothing
/// else on the runtime.
///
/// A window boundary can land mid-line. When the seek away from the start
/// of the file was non-zero, the bytes up to and including the first `\n`
/// in the window are discarded rather than rendered as a fragment — half a
/// line shown as a whole one is a lie. The remaining bytes are decoded with
/// [`String::from_utf8_lossy`]: a log file is whatever the child wrote and
/// is under no obligation to be UTF-8, and refusing to show a log over one
/// bad byte is the wrong failure.
///
/// # Errors
/// The file could not be opened, `stat`ed, seeked, or read. Notably
/// includes [`io::ErrorKind::NotFound`] (the sheep has never run in this
/// `$SHEP_HOME`) and `EISDIR` (`out_file`/`err_file` named a directory) —
/// [`tail_log_files`] gives the two different treatment.
pub(crate) fn read_tail(path: &Path, limit: usize) -> io::Result<(Vec<String>, bool)> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(TAIL_WINDOW_BYTES);
    // `start > 0` means the byte window itself left content behind, before
    // a single line has been counted — the file is bigger than the window.
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
    let mut lines: Vec<String> = text.split('\n').map(String::from).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let keep_from = lines.len().saturating_sub(limit);
    let truncated = window_truncated || keep_from > 0;
    lines.drain(..keep_from);
    Ok((lines, truncated))
}

/// Renders the selected files of every sheep the selector admits, in id
/// order, and returns the exit code that reports how that went.
///
/// Within one sheep, `out_file` (unless `--err`) prints before `err_file`
/// (unless `--out`) — this module's own doc states the ordering limitation
/// that follows from it. The matched sheep are sorted before anything is
/// read -- `cache` is a `HashMap` and its iteration order is arbitrary -- by
/// name and then by id, the one order every operator-facing listing takes
/// (`shep_core::protocol::sort_flock`). It was id order until that rule was
/// made the only one.
///
/// A `None` path means the shepherd predates the field (module doc,
/// [`shep_core::protocol::ProcessInfo::out_file`]) — one `log_path_unknown`
/// notice per path the flags actually asked for, exit code unaffected. A
/// missing file ([`io::ErrorKind::NotFound`]) is silent: the daemon creates
/// both files at spawn, so a missing one means this sheep has never run in
/// this `$SHEP_HOME`, and a notice per quiet sheep would spam stderr on a
/// fresh flock. Any other read failure is one `log_unreadable` notice naming
/// the path and the OS error, and the rest of the flock still prints — only
/// this last case sets the final [`ExitCode::Failure`].
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
    // Name then id, the one order every operator-facing shep listing takes
    // (`shep_core::protocol::sort_flock`'s own doc). Not `sort_flock` itself:
    // this is a `Vec<&ProcessInfo>` borrowed out of the cache, and copying the
    // rows to reach the helper would buy nothing but a clone per sheep.
    matched.sort_unstable_by(|a, b| (a.name.as_str(), a.id).cmp(&(b.name.as_str(), b.id)));

    let mut failure = false;

    // One file, one read. Several instances can resolve to one path: every
    // `merge_logs` app does, and so does any app that set `out_file`
    // explicitly. Reading per row printed the file once per instance.
    let mut seen_paths: HashSet<String> = HashSet::new();
    let mut seen_notices: HashSet<String> = HashSet::new();

    // Whether a path is shared between several rows, over the WHOLE cache
    // rather than the matched subset -- a selector narrowing to one row
    // must not hide that the underlying file is still shared, since the
    // file itself has no idea which lines are whose either way.
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
                    // belongs to exactly one row: several instances sharing
                    // one file (`merge_logs`, or a hand-set `out_file`)
                    // interleave in it and no line says who wrote it, so
                    // shep does not guess.
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
/// `BusEvent` is `#[non_exhaustive]`: the `_` arm ignores anything this
/// client does not recognise, silently — a follow must not die on a bus
/// event a newer daemon added (Global Constraints). `Dropped` is NOT one of
/// those unrecognised events — it is a real, named variant this client
/// understands — so it gets its own arm rather than falling into that `_`.
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
            // Daemon-side cause, deliberately NOT the `Lagged` arm's
            // wording below: `Dropped` is the daemon's own outbound queue
            // overflowing for this subscriber, while `Lagged` is this
            // client's receiver falling behind reading its socket. The two
            // failures live on opposite sides of the connection and must
            // read differently, or a user cannot tell which end to
            // investigate.
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
/// `quiet` is `cli::GlobalArgs::quiet` — it silences this module's own
/// notices (whole-branch review item 2) and nothing else: a sheep's own
/// line and a real error both still print regardless.
///
/// Delegates to [`bleats_with_signal`] with a real `SIGINT` as the
/// interrupt source — see that function's own doc for the shape both
/// share.
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
/// that does not need a real `SIGINT` — one would kill the test runner.
///
/// One `Request::ListFlock` always goes out first, building the id -> name
/// cache both paths share.
///
/// **`--no-follow`** (`args.no_follow`) stops there and hands off to
/// [`tail_log_files`]: it never issues `Request::Subscribe`, so there is no
/// stream, no `Lagged`, no `DaemonShutdown`, and nothing for `interrupt` to
/// race — a bounded file read terminates on its own.
///
/// **`--follow`** (the default) subscribes (`log.*`/`daemon.*`) and loops
/// over one `tokio::select!` with two arms, checked in this priority order
/// every iteration:
///
/// 1. The event stream — a normal line is rendered, a `Lagged` item is
///    noted to `streams.err` and the follow continues, and the stream
///    ending (`None`) means the daemon is gone: flush and exit
///    [`ExitCode::DaemonUnreachable`].
/// 2. `interrupt` — a user ending a follow deliberately has not failed:
///    flush and exit [`ExitCode::Success`].
///
/// `streams.out` is flushed on every exit path — a follow that ends with
/// lines still buffered would otherwise lose them silently.
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

    // Order matters: the id/name cache is built from ONE listing taken
    // before subscribing. Subscribing first would lose every line pushed
    // while the listing is still in flight.
    let cache = match resolve_names(client, streams).await {
        Ok(cache) => cache,
        Err(code) => return code,
    };

    if args.no_follow {
        return tail_log_files(streams, quiet, &cache, &selector, args);
    }

    // The backlog first, then the stream. Following alone shows only what
    // arrives next, so a sheep that already died printed an empty screen
    // while the reason sat in its log file -- which is how a boot-looping
    // sheep came to look like it had logged nothing at all.
    //
    // Read before subscribing rather than after, so a line written in the
    // gap between the two is missed rather than printed twice. Neither is
    // free; a duplicate is the one a reader would notice and mistrust. The
    // same trade is already made just above, where the id/name cache is
    // built from a listing taken before the subscription.
    //
    // The tail's own exit code is deliberately discarded: an unreadable log
    // for one sheep must not stop a follow over the whole flock, and a
    // failed write to stdout will fail again in the loop below, where it is
    // handled properly.
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

    /// Like [`info`], but with a real instance slot -- `info` never sets
    /// one, so it cannot express a multi-instance app on its own.
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
            // The tail default is exercised by its own tests below. The
            // follow tests here predate the backlog and assert on what the
            // BUS delivers, so they ask for no history and keep testing
            // exactly what they were written to test.
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

    /// Writes `content` to `dir/name` and returns the path as a `String` —
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
    /// interrupt) fails with a named assertion instead of a killed CI job.
    const RUN_TIMEOUT: Duration = Duration::from_secs(5);

    /// `daemon.close_after_subscribe()` — not `daemon.close()` — ends the
    /// connection right after the real `Subscribe` this test's `bleats`
    /// call issues has been served and anything queued via `push` has been
    /// flushed. That is what makes this test scheduler-independent: a
    /// follow that runs to end-of-stream observes everything pushed before
    /// the close, in order, on any executor — there is no race between "the
    /// events arrived" and "the loop decided nothing was buffered".
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

    /// The daemon's topic filter globs on `log.out` / `log.err`, which carry
    /// no identity — so this filtering CANNOT have happened server-side,
    /// and a test that let the fake daemon pre-filter would prove nothing.
    /// Same `close_after_subscribe` reasoning as the test above.
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

    /// D11's whole asymmetry: unlike the backlog, a follow always knows
    /// which sheep wrote a line -- the daemon emits `BusEvent::LogOut` per
    /// sheep -- so it labels a multi-instance app's lines with their slot
    /// even though `info_with_instance`'s two rows would share a file if
    /// this went through the backlog path. `resolved_instance` is the only
    /// thing that can produce this label; dropping its `instance_count`
    /// gate, or failing to thread `instance` into `handle_event`'s
    /// `LogOut`/`LogErr` arms, both turn this red without touching a single
    /// other test in this module (see the fix report for the manual check).
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

    /// Minor fix bundle: a selector narrowed to one instance must not change
    /// how that instance's line is labelled -- `instance_count` counts over
    /// the WHOLE cache, never the matched subset, so `web:0` still prints
    /// `web:0` even though the cache holds a `web:1` this selector excludes.
    /// This deliberately differs from the flock table's own rollup rule,
    /// where the count describes the rows actually listed -- a table row
    /// summarises a listing, a log prefix identifies a process.
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

    /// The stream stays open for the whole test — the fake is never closed
    /// — so the ONLY thing that can end this follow is the injected
    /// interrupt. A `bleats` that ignored the interrupt arm hangs and the
    /// timeout fails it.
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

    /// The pair that makes the shutdown branch bite. Both end in
    /// `DaemonUnreachable` — the daemon went away either way — so the exit
    /// code alone discriminates nothing. The NOTICE is the behaviour under
    /// test: a `bleats` that never matches `BusEvent::DaemonShutdown` and
    /// just maps any end-of-stream to `DaemonUnreachable` passes the first
    /// assertion of each and fails the stderr assertion of the first.
    ///
    /// `close_after_subscribe`, not `close()`: this test genuinely needs
    /// the connection to end mid-follow (there is no interrupt here, and
    /// `follow_args` never terminates on its own), and `close_after_subscribe`
    /// ends it deterministically, right after the real `Subscribe` this
    /// test's `bleats` call issues has been served.
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

    /// Same `close_after_subscribe` usage as the test above, and for the
    /// same reason.
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

    /// Whole-branch review item 2: `--quiet` (`GlobalArgs::quiet`, threaded
    /// in here as `bleats`' own `quiet` parameter) must actually do
    /// something, and this module's notices are what it was given meaning
    /// against. Same shutdown scenario as
    /// `a_daemon_shutdown_mid_follow_is_announced_before_the_stream_ends`,
    /// with `quiet: true` instead of `false` — the exit code must not move
    /// (the daemon really did go away either way), only the notice text.
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

    /// The other half of `--quiet`'s contract: it narrows notices only.
    /// `resolved_name`/`write_line` never see `quiet` at all (their call
    /// sites are unconditional in `handle_event`), so this is a
    /// belt-and-braces end-to-end check that a sheep's own line still
    /// reaches `streams.out` under `quiet: true`.
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

    /// Same `close_after_subscribe` reasoning as
    /// `ids_resolve_to_names_from_one_listing_and_unknown_ids_render_bare` —
    /// but unlike that test, this one is **not** scheduler-independent, for
    /// a different reason than the drain arm the amendment retired.
    ///
    /// **Verified, current known limitation**: run 10/10 in isolation under
    /// `#[tokio::test(flavor = "multi_thread")]`, this test fails 10/10 —
    /// `stderr` comes back empty, meaning `overrun_by`'s forced lag never
    /// happens. `overrun_by` pushes `EVENT_CHANNEL_CAPACITY + n` events that
    /// the fake flushes onto the wire in one burst right after `Subscribe`;
    /// a `Lagged` only appears if this test's own `EventStream` falls behind
    /// that burst by more than `EVENT_CHANNEL_CAPACITY` before it drains any
    /// of it. Under the default current-thread runtime that reliably
    /// happens: the connection actor decodes the whole burst in one
    /// uninterrupted turn before this test's task gets scheduled again.
    /// Under `multi_thread`, real parallelism lets this test's receiver keep
    /// pace with the actor as events arrive, so the backlog never crosses
    /// `EVENT_CHANNEL_CAPACITY` and no lag is ever produced. The other six
    /// tests converted alongside this one all pass 10/10 in the same
    /// isolated check — this is `overrun_by`'s own timing dependency, not a
    /// symptom of the retired drain arm, and forcing a deterministic lag
    /// would need a synchronization point `FakeDaemon` does not have today.
    /// `cfg(unix)` on top of the scheduling dependency this test's own doc
    /// already describes above. `overrun_by` produces a lag only when the
    /// connection actor decodes the whole burst in one uninterrupted turn
    /// before this task is scheduled again — the doc says so, and says that
    /// a deterministic version would need a synchronization point
    /// `FakeDaemon` does not have.
    ///
    /// A named pipe wakes its reader on a different schedule than a unix
    /// socket does, so on Windows the receiver keeps pace and the backlog
    /// never crosses `EVENT_CHANNEL_CAPACITY` — the same way the doc
    /// records `multi_thread` defeating it. That is this fixture's known
    /// fragility meeting a second transport, not a lag notice that stopped
    /// working: `EventStream`'s own `Lagged` handling is covered by
    /// `shep-client`'s tests, which run on both platforms.
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

    /// fails if `BusEvent::Dropped` falls into `handle_event`'s `_ => Ok(())`
    /// catch-all and vanishes — the daemon's own outbound queue overflowing
    /// is exactly the "a sheep went quiet" failure mode this module's doc
    /// warns against swallowing silently.
    ///
    /// `Dropped` (the daemon's queue) and `Lagged` (this client's own
    /// receiver falling behind) are different causes and must read
    /// differently, so this asserts the daemon-side wording specifically —
    /// `stderr.contains("dropped")` alone would also pass if the `Lagged`
    /// arm's wording were reused by mistake, which is exactly the bug this
    /// test exists to catch.
    ///
    /// Same `close_after_subscribe` reasoning as
    /// `ids_resolve_to_names_from_one_listing_and_unknown_ids_render_bare`.
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

    /// Important fix (item 3): every other test in this module uses
    /// `Format::Table`, so mutating the JSON line shape (renaming a field,
    /// or rendering table rows under `--format json`) left every test green.
    /// Global Constraints pins every command's JSON shape; this is that pin
    /// for `bleats`' own line shape (deferred by the brief to a Task 12
    /// fixture, pinned independently here since item 2 puts that fixture's
    /// shape in doubt).
    ///
    /// Same `close_after_subscribe` reasoning as
    /// `ids_resolve_to_names_from_one_listing_and_unknown_ids_render_bare`.
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
    /// exercised that path through an actual write failure.
    ///
    /// `close_after_subscribe` is scripted here too, for consistency with
    /// the rest of this module's follow-mode tests, but the exit code stays
    /// `Success` regardless: the write fails on the very first event, which
    /// returns from `handle_event`'s write-error branch long before the
    /// stream could ever reach end-of-stream.
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

    // --- Task 10a: `--no-follow` reads the log files ----------------------
    //
    // None of these subscribe: `--no-follow` never issues `Request::Subscribe`
    // at all, so there is no `daemon.close()`/`close_after_subscribe()` to
    // script and no scheduler dependence to worry about — a bounded file read
    // terminates on its own, which is exactly what `RUN_TIMEOUT` guards.

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

    /// The bug the maintainer hit: a sheep crashes, `shep bleats <name>` is run after
    /// the fact, and following alone prints an empty screen while the reason
    /// sits in the log file. The backlog is what makes the reason reachable
    /// without having to start the sheep again in a second window.
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

    /// `truncated` must report `true` when the byte window is what cut the
    /// tail short, even when the line cap never binds — a sheep logging a
    /// few long, structured lines can fill `TAIL_WINDOW_BYTES` in far fewer
    /// than `limit` lines. Before this test, `read_tail` reported `false`
    /// here, and `whistle`'s `tail_bleats` handed a model exactly that
    /// wrong answer.
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
    /// neither (out lines, then err lines — this module's own pin on
    /// within-sheep ordering).
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

    /// fails if the sheep are tailed in the order the cache happens to hold
    /// them, or in id order.
    ///
    /// The fixture is built so those two answers and the right one are all
    /// different. `b` holds id 1 and `a` holds id 2, so id order is `b, a`
    /// while name order is `a, b`; the listing is scripted `b, a` so the
    /// cache's arbitrary `HashMap` order cannot be what makes the assertion
    /// pass either.
    ///
    /// If id order and name order ever match, the test passes without being
    /// able to tell the two apart, so the fixture must keep them
    /// disagreeing.
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

    /// The daemon creates both files at spawn, so a missing one means this
    /// sheep has never run in this `$SHEP_HOME` — not a fault worth a
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
