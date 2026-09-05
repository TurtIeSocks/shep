//! Log-plane verbs: the ones that act on a sheep's log files.
//!
//! Reading a sheep's output is `commands::bleats`, which is what `shep logs`
//! aliases despite this module's name. Here, `reopen` hands the files to an
//! external rotator that has renamed them and `flush` empties them in place.
//!
//! The child never sees its log file: it is spawned with `Stdio::piped()`
//! and the daemon does the file I/O, so nothing here asks anything of the
//! child.

use std::time::Duration;

use shep_client::{Client, LOG_PLANE_DEADLINE};
use shep_core::paths::ShepPaths;
use shep_core::protocol::{Request, Response, SelectorSpec};

use crate::cli::{FlushArgs, ReopenArgs};
use crate::commands::selector::parse_selector;
use crate::exit::ExitCode;
use crate::launch;
use crate::output::{
    EmptiedFile, EmptiedFiles, FlockRows, FlushedRows, Render, Streams, emit, write_outcome,
};

/// Sends `body` with `deadline` (`None` defers to the client's own default),
/// renders whatever the daemon answers through [`emit`], and maps every way
/// that can go wrong to its exit code.
///
/// `extract` pulls the verb's own payload out of `Response`, which is
/// `#[non_exhaustive]`: an answer it does not recognise maps to
/// [`ExitCode::Internal`] rather than being guessed at.
async fn request_and_render<T, F>(
    client: &Client,
    streams: &mut Streams<'_>,
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
            Some(payload) => write_outcome(emit(
                &mut *streams.out,
                streams.fmt,
                command,
                payload,
                streams.style,
            )),
            None => {
                let message = "the daemon answered with a response this client does not understand";
                streams.fail(ExitCode::Internal, message)
            }
        },
        Err(err) => {
            let code = ExitCode::from(&err);
            streams.fail(code, &err.to_string())
        }
    }
}

/// Reopens the log files of the sheep matching `args.selector`, for an
/// external rotator that has renamed them.
///
/// A zero exit means every matched sheep's pump holds a handle on the
/// recreated path, so a `postrotate` stanza waiting on this command knows no
/// live pump is still filling the archive. The daemon reaches every writer to
/// a path it rotates, not only the sheep named: several instances can share
/// one file. A sheep that is not running has no pump and is reported like the
/// rest; a pump that cannot open its path again fails the command.
///
/// Sent with [`LOG_PLANE_DEADLINE`]: the daemon visits matched sheep serially
/// with no per-sheep bound, so the client's 5s default would time out.
pub async fn reopen(client: &Client, streams: &mut Streams<'_>, args: &ReopenArgs) -> ExitCode {
    let selector = match parse_selector(streams, &args.selector) {
        Ok(selector) => SelectorSpec::from(&selector),
        Err(code) => return code,
    };
    request_and_render(
        client,
        streams,
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
/// flushes what every pump owes those files, then truncates the paths the
/// sheep were registered with, running or not.
///
/// Those paths are ordinary config values, never checked against the log
/// directory, so an `out_file` naming something that is not a log is emptied
/// too, with the shepherd's privileges. Several sheep can share a path: it is
/// truncated once, and a sharing sheep the selector skipped is emptied too.
///
/// The shepherd's own two logs are out of reach of any selector and are
/// [`flush_daemon`]'s. Sent with [`LOG_PLANE_DEADLINE`], as [`reopen`] is.
pub async fn flush(client: &Client, streams: &mut Streams<'_>, args: &FlushArgs) -> ExitCode {
    // `main` routes `--daemon` away and clap requires a selector otherwise,
    // so this arm is a guard against a dispatch bug.
    let Some(raw) = args.selector.as_deref() else {
        return streams.fail(
            ExitCode::Usage,
            "flush needs a selector, or --daemon for the shepherd's own logs",
        );
    };
    let selector = match parse_selector(streams, raw) {
        Ok(selector) => SelectorSpec::from(&selector),
        Err(code) => return code,
    };
    request_and_render(
        client,
        streams,
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
/// The CLI owns these files: `launch::launch_command` creates them before the
/// daemon exists and hands them over as fds 1 and 2, so the daemon knows no
/// path for them. This is the one flush that works with the shepherd down.
///
/// No flush barrier, unlike the flock half: the daemon's records reach fd 2
/// synchronously. The next line lands at offset 0 because
/// [`launch::launch_command`] opens both files `O_APPEND`; a daemon holding
/// a descriptor from elsewhere keeps writing at the offset it remembers. A
/// missing file is reported `absent` rather than created.
pub fn flush_daemon(streams: &mut Streams<'_>, paths: &ShepPaths) -> ExitCode {
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
        return streams.fail(ExitCode::Failure, &failures.join("; "));
    }
    write_outcome(emit(
        &mut *streams.out,
        streams.fmt,
        "flush",
        EmptiedFiles(emptied),
        streams.style,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Format;
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

    #[tokio::test]
    async fn every_selector_form_reaches_the_wire_inside_a_reopen_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
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
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            let _ = reopen(&client, &mut streams, &args(input)).await;
            let sent = envelopes.recv().await.unwrap();
            assert_eq!(
                sent.body,
                Request::Reopen { selector: expected },
                "input={input}"
            );
        }
    }

    /// The literal `30_000` is here and only here: comparing against
    /// [`LOG_PLANE_DEADLINE`] alone would hold whatever the constant says.
    #[tokio::test]
    async fn a_reopen_asks_for_the_longer_deadline() {
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

        let _ = reopen(&client, &mut streams, &args("all")).await;

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

    /// `"/[/"` is one of only three inputs the selector grammar rejects.
    #[tokio::test]
    async fn a_malformed_selector_exits_usage_without_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            reopen(&client, &mut streams, &args("/[/")).await
        };
        assert_eq!(code, ExitCode::Usage);
        assert!(
            envelopes.try_recv().is_err(),
            "a malformed selector must fail locally"
        );
    }

    #[tokio::test]
    async fn a_not_found_reply_exits_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, _served) =
            fake_client_replying_err(&path, RpcErrorCode::NotFound, "no sheep matched").await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
            style: crate::style::Presentation::BARE,
            fmt: Format::Table,
        };
        let code = reopen(&client, &mut streams, &args("ghost")).await;
        assert_eq!(code, ExitCode::NotFound);
    }

    #[tokio::test]
    async fn every_selector_form_reaches_the_wire_inside_a_flush_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
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
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            let _ = flush(&client, &mut streams, &flush_args(input)).await;
            let sent = envelopes.recv().await.unwrap();
            assert_eq!(
                sent.body,
                Request::Flush { selector: expected },
                "input={input}"
            );
        }
    }

    #[tokio::test]
    async fn a_flush_asks_for_the_longer_deadline() {
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

        let _ = flush(&client, &mut streams, &flush_args("all")).await;

        let sent = envelopes.recv().await.unwrap();
        assert_eq!(
            sent.deadline_ms,
            Some(u64::try_from(LOG_PLANE_DEADLINE.as_millis()).unwrap())
        );
    }

    /// The bare `try_recv` needs no bounded wait: `flush` returns only after
    /// a full round trip, so a send would already have been captured.
    #[tokio::test]
    async fn a_malformed_flush_selector_exits_usage_without_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, mut envelopes) = fake_client_capturing_envelopes(&path).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = {
            let mut streams = Streams {
                out: &mut out,
                err: &mut err,
                style: crate::style::Presentation::BARE,
                fmt: Format::Table,
            };
            flush(&client, &mut streams, &flush_args("/[/")).await
        };
        assert_eq!(code, ExitCode::Usage);
        assert!(
            envelopes.try_recv().is_err(),
            "a malformed selector must fail locally"
        );
    }

    /// `NotFound` is the only refusal a fake client can produce here.
    #[tokio::test]
    async fn a_refused_flush_never_exits_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = shep_client::testing::control_address(dir.path());
        let (client, _served) =
            fake_client_replying_err(&path, RpcErrorCode::NotFound, "no sheep matched").await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
            style: crate::style::Presentation::BARE,
            fmt: Format::Table,
        };
        let code = flush(&client, &mut streams, &flush_args("ghost")).await;
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

    #[test]
    fn a_daemon_flush_empties_both_shepherd_logs_and_names_them() {
        let dir = tempfile::tempdir().unwrap();
        let paths = home_with_daemon_logs(&dir, b"old shepherd output");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = Streams {
            out: &mut out,
            err: &mut err,
            style: crate::style::Presentation::BARE,
            fmt: Format::Json,
        };

        let code = flush_daemon(&mut streams, &paths);

        assert_eq!(code, ExitCode::Success);
        for name in [launch::DAEMON_STDOUT_LOG, launch::DAEMON_STDERR_LOG] {
            let path = paths.logs.join(name);
            assert_eq!(std::fs::metadata(&path).unwrap().len(), 0, "{name}");
            // JSON-encoded, not raw: on Windows a path's separators are
            // escaped inside the envelope.
            let needle = serde_json::to_string(&path.display().to_string()).unwrap();
            let needle = needle.trim_matches('"');
            assert!(
                String::from_utf8_lossy(&out).contains(needle),
                "the answer must name every file it emptied: {}",
                String::from_utf8_lossy(&out)
            );
        }
    }

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
            style: crate::style::Presentation::BARE,
            fmt: Format::Json,
        };

        let code = flush_daemon(&mut streams, &paths);

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

    /// A directory in the log's place fails `open(2)` for writing for every
    /// uid, root included.
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
            style: crate::style::Presentation::BARE,
            fmt: Format::Table,
        };

        let code = flush_daemon(&mut streams, &paths);

        assert_eq!(code, ExitCode::Failure);
        assert!(
            String::from_utf8_lossy(&err).contains(&blocked.display().to_string()),
            "the failure must name the file it could not empty: {}",
            String::from_utf8_lossy(&err)
        );
    }
}
