//! Log-plane verbs: the ones that act on a sheep's log FILES.
//!
//! Reading a sheep's log output is `bleats` and lives in
//! `commands::bleats` — which is what `shep logs` aliases, despite this
//! module's name. This module is the other half: the files themselves.
//! `reopen` hands them to an external rotator that has renamed them;
//! `flush` empties them in place.
//!
//! Like every other verb module, these receive an already-connected
//! [`Client`]; `main` connects, and nothing here autostarts a daemon.
//!
//! # Why acting on a sheep's log files is cheap
//!
//! The child never sees its log file. It is spawned with `Stdio::piped()`
//! and the daemon does the file I/O on the far side of that pipe, so
//! swapping the daemon's handle — or emptying the file under it — is
//! invisible across the process boundary: no signal to the child, no fd
//! surgery, no restart, and no gap in the pipe. Nothing child-side is
//! needed to rotate or clear a sheep's logs, so nothing here asks anything
//! of the child.

use std::time::Duration;

use shep_client::{Client, LOG_PLANE_DEADLINE};
use shep_core::paths::ShepPaths;
use shep_core::protocol::{Request, Response, SelectorSpec};

use crate::cli::{FlushArgs, Format, ReopenArgs};
use crate::commands::selector::parse_selector;
use crate::exit::ExitCode;
use crate::launch;
use crate::output::{
    EmptiedFile, EmptiedFiles, FlockRows, FlushedRows, Render, Streams, emit, emit_error,
    write_outcome,
};

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

/// Reopens the log files of the sheep matching `args.selector`, for an
/// external rotator that has renamed them.
///
/// A zero exit means every matched sheep's log pump holds a handle on the
/// recreated path, so a `postrotate` stanza that waits for this command
/// knows no live pump is still filling the archive it just renamed. That
/// holds because the daemon reaches every writer to a path it is rotating,
/// not only the sheep named here — several instances can share one file, and
/// one of them left unasked would go on filling the archive. A matched sheep
/// that is not running has no pump and nothing to reopen; it is reported
/// alongside the rest rather than as a failure.
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
/// Sent with [`LOG_PLANE_DEADLINE`] rather than the client's default, the way
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
        Ok(selector) => SelectorSpec::from(&selector),
        Err(code) => return code,
    };
    request_and_render(
        client,
        streams,
        fmt,
        "reopen",
        Request::Reopen { selector },
        Some(LOG_PLANE_DEADLINE),
        |response| match response {
            Response::Reopened(procs) => Some(FlockRows(procs)),
            _ => None,
        },
    )
    .await
}

/// Empties the log files of the sheep matching `args.selector`: the daemon
/// flushes what every pump writing to one of those files still owes it, then
/// truncates the paths those sheep were registered with.
///
/// # What gets emptied
///
/// Exactly the paths the Flockfile names — `out_file` and `err_file` as the
/// daemon resolved them — for every registered sheep the selector matches,
/// whether or not it has ever run. Those are ordinary config values, taken
/// verbatim and never checked against the log directory, so an app pointing
/// `out_file` at something that is not a log file makes this verb empty that
/// file too, with the shepherd's privileges.
///
/// # Why the selector is required
///
/// This destroys log data, so it follows `stop`/`restart`/`delete` in
/// demanding a target — where `bleats` and `reopen` default to `all` because
/// neither destroys anything. `shep flush` with no argument is a usage error,
/// not "empty every log in the flock": that is the one command here whose
/// slip of the finger cannot be undone, and `shep flush all` is a short thing
/// to type when it is meant. [`FlushArgs`] carries the selector as an
/// `Option` only because `--daemon` names a target that is not a selection at
/// all; clap still refuses a `flush` that names neither.
///
/// # A stopped sheep is emptied too
///
/// The daemon truncates recorded paths, not open handles, so a matched sheep
/// with no running process is emptied like any other. That is the useful
/// behaviour rather than an accident of the implementation: a stopped sheep's
/// logs are still readable with `shep bleats --no-follow`, so they are still
/// worth being able to clear.
///
/// # What this does NOT touch
///
/// The shepherd's own `shepd.out.log`/`shepd.err.log`. Those are opened by
/// the CLI's launcher before the daemon exists, and the daemon inherits them
/// as plain fds 1 and 2. It holds no handle it could flush and no recorded
/// path it could truncate, so no selector can reach them and none ever will:
/// they are [`flush_daemon`]'s, reached only by naming `--daemon`.
///
/// Renders the matched sheep as [`FlushedRows`] — one row per SHEEP, and the
/// two paths that sheep contributed. Not [`FlockRows`], which every other
/// flock-shaped verb answers with: those keep `out_file`/`err_file` out of the
/// table for being too wide, and here they are the answer. A verb that empties
/// files an operator may have mistyped, and then reports lifecycle columns it
/// did not touch, has told them nothing about what it destroyed. The JSON is
/// unchanged either way — the paths were always in it.
///
/// Several sheep can share one log path (`merge_logs`, or an explicit
/// `out_file` on a multi-instance app) and the daemon truncates each distinct
/// path once, so the same path can appear in two rows. A sharing sheep the
/// selector skipped has that file emptied under it all the same, with its pump
/// flushed first so none of its pending lines lands in the file afterwards —
/// it is not a row here, because it is not a sheep the operator named, and
/// that is the one thing this table cannot show.
///
/// Sent with [`LOG_PLANE_DEADLINE`] for the reason [`reopen`] gives: the
/// daemon walks the matched flock file by file with no per-sheep bound.
///
/// # A selector-less call
///
/// `main` routes `--daemon` to [`flush_daemon`] before reaching here, and
/// clap's `required_unless_present` covers everything else, so no real
/// invocation arrives without a selector. The `None` arm below answers with
/// the usage error clap itself would rather than an `expect`: a panicking
/// convenience would abort the process over a bug in the dispatch above, and
/// buy an operator nothing that one branch does not.
pub async fn flush(
    client: &Client,
    streams: &mut Streams<'_>,
    fmt: Format,
    args: &FlushArgs,
) -> ExitCode {
    let Some(raw) = args.selector.as_deref() else {
        let _ = emit_error(
            &mut *streams.err,
            fmt,
            ExitCode::Usage.code_str(),
            "flush needs a selector, or --daemon for the shepherd's own logs",
        );
        return ExitCode::Usage;
    };
    let selector = match parse_selector(streams, fmt, raw) {
        Ok(selector) => SelectorSpec::from(&selector),
        Err(code) => return code,
    };
    request_and_render(
        client,
        streams,
        fmt,
        "flush",
        Request::Flush { selector },
        Some(LOG_PLANE_DEADLINE),
        |response| match response {
            Response::Flushed(procs) => Some(FlushedRows(procs)),
            _ => None,
        },
    )
    .await
}

/// Empties the shepherd's own two log files: `shep flush --daemon`.
///
/// # Why the CLI does this and not the daemon
///
/// It is the CLI that owns these files. `launch::launch_command` creates them
/// before the daemon exists and hands them over as fds 1 and 2; the daemon
/// never learns their paths, holds no `LogFile` for them, and has nothing to
/// answer a `Request::Flush` about. Truncating them here needs no new wire
/// variant, and — the useful consequence — needs no daemon either: this is
/// the one flush that works while the shepherd is down, which is when an
/// operator most often wants it.
///
/// # Why there is no flush barrier
///
/// The flock half exists in two phases because a pump's `write_all` returns
/// with the real `write(2)` still queued on the blocking pool, so a truncate
/// can outrun a line already dispatched. Nothing here is queued: the daemon's
/// records go through its `tracing` subscriber straight to fd 2, synchronously
/// on the thread that emitted them. There is no in-flight write to wait for,
/// and no channel to wait on it with.
///
/// # Where the next line lands
///
/// At offset 0, because [`launch::launch_command`] opens both files
/// `O_APPEND` — see `launch`'s own `emptied_appending` for the measurement,
/// and for why `File::create` (which is what this used to be) would instead
/// leave the daemon writing past a `NUL` hole the size of everything emptied. A
/// daemon launched by an older `shep` binary, or run in the foreground with
/// the operator's own shell redirection, keeps whatever descriptor it was
/// given: this verb still empties the file and still frees the disk blocks,
/// but that daemon's next line lands at its own remembered offset.
///
/// # A missing file is a success
///
/// The same rule the flock half applies to a sheep that has never run: a log
/// file that is not there is already empty. It is reported as `absent` rather
/// than created, so an operator can tell "emptied 4 MB" from "there was
/// nothing here" — and a `shep flush --daemon` against a cold `$SHEP_HOME`
/// exits 0 rather than complaining about a daemon that has never started.
pub fn flush_daemon(streams: &mut Streams<'_>, fmt: Format, paths: &ShepPaths) -> ExitCode {
    let mut emptied = Vec::new();
    let mut failures = Vec::new();

    for (stream, name) in [
        ("stdout", launch::DAEMON_STDOUT_LOG),
        ("stderr", launch::DAEMON_STDERR_LOG),
    ] {
        let path = paths.logs.join(name);
        let result = match std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
        {
            Ok(_) => "emptied",
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => "absent",
            Err(error) => {
                failures.push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        emptied.push(EmptiedFile {
            stream,
            file: path.display().to_string(),
            result,
        });
    }

    if !failures.is_empty() {
        let _ = emit_error(
            &mut *streams.err,
            fmt,
            ExitCode::Failure.code_str(),
            &failures.join("; "),
        );
        return ExitCode::Failure;
    }
    write_outcome(emit(&mut *streams.out, fmt, "flush", EmptiedFiles(emptied)))
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

    fn flush_args(selector: &str) -> FlushArgs {
        FlushArgs {
            selector: Some(selector.to_string()),
            daemon: false,
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
    ///
    /// The literal `30_000` is here and only here. Comparing against
    /// [`LOG_PLANE_DEADLINE`] alone is the assert-X-equals-X shape: it holds
    /// whatever the constant says, so the constant could be cut back to the
    /// 5s this verb is meant to escape and every deadline assertion in this
    /// module would still pass. One test names the number so that change has
    /// to be deliberate; the flush case below keeps comparing against the
    /// constant, which is what pins the two verbs to the same budget.
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
            Some(u64::try_from(LOG_PLANE_DEADLINE.as_millis()).unwrap())
        );
        assert_eq!(
            sent.deadline_ms,
            Some(30_000),
            "the log plane's budget is 30s; a shorter one is the regression \
             this verb exists to avoid, not a tuning choice"
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

    /// The flush counterpart of
    /// [`every_selector_form_reaches_the_wire_inside_a_reopen_request`], and
    /// the whole `sent.body` is asserted for the same reason.
    ///
    /// Fails if `flush` sends the raw selector string, a selector it did not
    /// parse, or another verb's request kind. `Request::Reopen` is the
    /// dangerous mis-wiring here — the two verbs are neighbours in this
    /// module, take the same shaped payload and answer with the same shaped
    /// table, so a `flush` that sent a `Reopen` would swap every log handle
    /// in the flock, empty nothing at all, and print a table that looks
    /// exactly right while doing it.
    #[tokio::test]
    async fn every_selector_form_reaches_the_wire_inside_a_flush_request() {
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
            let _ = flush(&client, &mut streams, Format::Table, &flush_args(input)).await;
            let sent = envelopes.recv().await.unwrap();
            assert_eq!(
                sent.body,
                Request::Flush { selector: expected },
                "input={input}"
            );
        }
    }

    /// Fails if `flush` leaves the deadline to the client's default, for the
    /// reason [`a_reopen_asks_for_the_longer_deadline`] gives about its own
    /// verb: the daemon walks the matched flock file by file with no
    /// per-sheep bound, so a 5s budget expires mid-flush on a slow log
    /// directory. Asserted on the wire, where `deadline_ms` is what the
    /// daemon actually budgets from.
    ///
    /// The failure here is worse than a reopen's. A `flush` that timed out
    /// exits non-zero having ALREADY emptied some of the matched files, so
    /// the operator is told it failed about work that is not coming back.
    #[tokio::test]
    async fn a_flush_asks_for_the_longer_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };

        let _ = flush(&client, &mut streams, Format::Table, &flush_args("all")).await;

        let sent = envelopes.recv().await.unwrap();
        assert_eq!(
            sent.deadline_ms,
            Some(u64::try_from(LOG_PLANE_DEADLINE.as_millis()).unwrap())
        );
    }

    /// Fails if `flush` skips the client-side parse. `"/[/"` is one of the
    /// only three inputs the selector grammar rejects; without the local
    /// parse the daemon answers `NotFound` after a round trip, so the
    /// operator gets "no sheep matched" for what is really a typo.
    ///
    /// The bare `try_recv` needs no bounded wait to be sound: `flush` above
    /// has already returned, and it returns only after a full round trip, so
    /// a version of it that sent would have had its envelope captured before
    /// this line runs. There is no window here for the channel to be
    /// legitimately empty-but-about-to-fill.
    #[tokio::test]
    async fn a_malformed_flush_selector_exits_usage_without_a_round_trip() {
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
            flush(&client, &mut streams, Format::Table, &flush_args("/[/")).await
        };
        assert_eq!(code, ExitCode::Usage);
        assert!(
            envelopes.try_recv().is_err(),
            "a malformed selector must fail locally"
        );
    }

    /// Fails if the verb swallows a daemon-side refusal and exits 0.
    ///
    /// A zero exit over a refusal is the silent failure in its purest form:
    /// an operator who ran `shep flush` and saw 0 believes those files are
    /// empty. The refusal driven here is `NotFound`, which is what the daemon
    /// answers a selector that matched nothing — the one refusal this tier
    /// can produce, since a fake client answers whatever it is armed with and
    /// no real path is ever truncated behind it.
    ///
    /// The refusal that costs the most, a path the daemon could not truncate,
    /// answers `Internal` and exits 9. Nothing here can provoke it, so it is
    /// pinned where it can be: `rpc::tests`'s
    /// `a_log_plane_failure_is_internal_and_says_which_half_failed` for the
    /// code and the message, and `cli_e2e`'s
    /// `a_reopen_that_cannot_open_a_path_again_exits_internal` for the exit
    /// status an operator's shell actually sees.
    #[tokio::test]
    async fn a_refused_flush_never_exits_zero() {
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
        let code = flush(&client, &mut streams, Format::Table, &flush_args("ghost")).await;
        assert_eq!(code, ExitCode::NotFound);
    }

    /// A `$SHEP_HOME` under `dir` with both shepherd log files present and
    /// holding `contents`.
    fn home_with_daemon_logs(dir: &tempfile::TempDir, contents: &[u8]) -> ShepPaths {
        let paths = ShepPaths::resolve(
            &|k| (k == "SHEP_HOME").then(|| dir.path().to_string_lossy().into_owned()),
            std::path::Path::new("/nonexistent"),
        );
        std::fs::create_dir_all(&paths.logs).unwrap();
        for name in [launch::DAEMON_STDOUT_LOG, launch::DAEMON_STDERR_LOG] {
            std::fs::write(paths.logs.join(name), contents).unwrap();
        }
        paths
    }

    /// Fails if `--daemon` stops emptying both of the shepherd's files, or
    /// stops naming them.
    ///
    /// The naming half is not decoration. The whole reason this verb reports
    /// paths at all is that an operator cannot otherwise tell WHAT a flush
    /// emptied, and for `--daemon` the two files are the entire answer — a
    /// table that said only "ok" would be indistinguishable from one that
    /// truncated the wrong `$SHEP_HOME`.
    #[test]
    fn a_daemon_flush_empties_both_shepherd_logs_and_names_them() {
        let dir = tempfile::tempdir().unwrap();
        let paths = home_with_daemon_logs(&dir, b"old shepherd output");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };

        let code = flush_daemon(&mut streams, Format::Json, &paths);

        assert_eq!(code, ExitCode::Success);
        for name in [launch::DAEMON_STDOUT_LOG, launch::DAEMON_STDERR_LOG] {
            let path = paths.logs.join(name);
            assert_eq!(std::fs::metadata(&path).unwrap().len(), 0, "{name}");
            assert!(
                String::from_utf8_lossy(&out).contains(&path.display().to_string()),
                "the answer must name every file it emptied: {}",
                String::from_utf8_lossy(&out)
            );
        }
    }

    /// Fails if a missing shepherd log becomes an error, or is created to
    /// make the row look tidy.
    ///
    /// A cold `$SHEP_HOME` has never had a daemon in it, so neither file
    /// exists — and a `shep flush --daemon` that exited non-zero there would
    /// be complaining that there was nothing to do. `absent` rather than
    /// `emptied` because the two are different facts: an operator chasing a
    /// full disk needs to know which files this actually truncated. The same
    /// rule the flock half applies to a sheep that has never run.
    #[test]
    fn a_daemon_flush_reports_a_missing_log_as_absent_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ShepPaths::resolve(
            &|k| (k == "SHEP_HOME").then(|| dir.path().to_string_lossy().into_owned()),
            std::path::Path::new("/nonexistent"),
        );
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };

        let code = flush_daemon(&mut streams, Format::Json, &paths);

        assert_eq!(code, ExitCode::Success);
        let envelope: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let rows = envelope["data"].as_array().unwrap();
        assert_eq!(
            rows.len(),
            2,
            "both streams are reported either way: {envelope}"
        );
        assert!(
            rows.iter().all(|row| row["result"] == "absent"),
            "a log that is not there is already empty: {envelope}"
        );
        assert!(
            !paths.logs.join(launch::DAEMON_STDOUT_LOG).exists(),
            "a flush must not create the log file it did not find"
        );
    }

    /// Fails if a file that could not be truncated is reported as emptied —
    /// the silent failure `a_refused_flush_never_exits_zero` pins for the
    /// flock half, in the half that never touches the socket. An operator who
    /// ran this to reclaim a full disk and saw 0 believes the space is back.
    ///
    /// A directory in the log's place is the failure with no permission games
    /// in it: `open(2)` for writing on a directory fails for every uid, root
    /// included, so this cannot pass for the wrong reason on a privileged
    /// runner.
    #[test]
    fn a_daemon_flush_that_could_not_empty_a_file_never_exits_zero() {
        let dir = tempfile::tempdir().unwrap();
        let paths = home_with_daemon_logs(&dir, b"");
        let blocked = paths.logs.join(launch::DAEMON_STDERR_LOG);
        std::fs::remove_file(&blocked).unwrap();
        std::fs::create_dir(&blocked).unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
        };

        let code = flush_daemon(&mut streams, Format::Table, &paths);

        assert_eq!(code, ExitCode::Failure);
        assert!(
            String::from_utf8_lossy(&err).contains(&blocked.display().to_string()),
            "the failure must name the file it could not empty: {}",
            String::from_utf8_lossy(&err)
        );
    }
}
