//! The foreground engine shared by `runtime` and `dev`: boots a shepherd in
//! this process, starts a flock, streams its bleats to stdout, and returns
//! once nothing is online or a signal ends the supervisor.

use shep_client::{Client, START_DEADLINE};
use shep_core::config::AppConfig;
use shep_core::paths::ShepPaths;
use shep_core::protocol::{Request, Response, SelectorSpec};

use crate::cli::{BleatsArgs, DaemonArgs};
use crate::commands::bleats::bleats_with_signal;
use crate::commands::daemon::{boot_supervisor, daemon_exit_code};
use crate::commands::empty::{Sample, sample, watch_until_empty};
use crate::exit::ExitCode;
use crate::output::Streams;

/// What the foreground engine should do, for the two verbs that use it.
pub struct ForegroundOptions {
    /// Where this flock lives. `dev` computes its own; `runtime` takes the
    /// ordinary `$SHEP_HOME`.
    pub paths: ShepPaths,
    /// Apps to start once the shepherd is up.
    pub apps: Vec<AppConfig>,
    /// Stop and delete the flock on the way out. `dev` does; `runtime` does
    /// not.
    pub tidy_up: bool,
}

/// How the race between the empty-flock watcher and the supervisor's own
/// task ended, captured while [`bleats_with_signal`] is still running.
enum Ending {
    /// [`watch_until_empty`] settled on a debounced reading.
    Empty(Sample),
    /// The supervisor task returned before the flock finished debouncing
    /// empty, which is how a SIGINT or SIGTERM arrives. `failed` is `true`
    /// when that return was `Err`, or the task panicked.
    SupervisorExited { failed: bool },
}

/// Refuses `client` if its shepherd disagrees with this binary's crate
/// version. [`run`] connects on its own, so the guard is applied here.
///
/// # Errors
/// [`ExitCode::VersionSkew`], as [`crate::refuse_version_skew`].
fn refuse_if_skewed(streams: &mut Streams<'_>, client: &Client) -> Result<(), ExitCode> {
    crate::refuse_version_skew(streams, client, crate::VersionGuard::Enforce)
}

/// Boots a shepherd in this process, starts `options.apps`, streams their
/// bleats to `streams.out`, and returns when nothing is online or a signal
/// arrives.
///
/// `streams` must hold unlocked handles, never a `StdoutLock`/`StderrLock`:
/// this runs until the flock empties, and a lock held across that wedges the
/// supervisor's own logging, written from a tokio worker thread in this same
/// process.
///
/// The shepherd is reached over its own socket, so `shep flock` from a second
/// terminal works. `shep_daemon::boot` already installs the SIGINT/SIGTERM
/// handlers that end the supervisor task; a second set here would race them.
pub async fn run(streams: &mut Streams<'_>, quiet: bool, options: ForegroundOptions) -> ExitCode {
    let ForegroundOptions {
        paths,
        apps,
        tidy_up,
    } = options;

    // `no_restore`: a container and a dev session both start from their
    // Flockfile, never from a saved roll.
    let daemon_args = DaemonArgs {
        cmd: None,
        no_restore: true,
        foreground: true,
        log_json: None,
        log_level: None,
        socket: None,
        max_cron_sleep: None,
    };

    // `tidy_up` doubles as `BootOptions::delete_flock_on_shutdown`: a session
    // ending by signal reaches `RunningDaemon::run`'s own teardown directly
    // and never runs the `Stop`/`Delete` pair below.
    let daemon = match boot_supervisor(paths.clone(), &daemon_args, tidy_up).await {
        Ok(daemon) => daemon,
        Err(err) => {
            let code = daemon_exit_code(&err);
            return streams.fail(code, &err.to_string());
        }
    };

    let mut supervisor = tokio::spawn(daemon.run());

    // The listener is bound by the time `boot_supervisor` returns, so this
    // needs no retry.
    let client = match Client::connect(&paths.socket).await {
        Ok(client) => {
            // `foreground` registers a sheep with the shepherd it just
            // booted, so it can never be one of `RECOVERY_VERBS`.
            if let Err(code) = refuse_if_skewed(streams, &client) {
                supervisor.abort();
                return code;
            }
            client
        }
        Err(err) => {
            supervisor.abort();
            let code = ExitCode::from(&err);
            return streams.fail(code, &err.to_string());
        }
    };

    if let Err(err) = client
        .request_with_deadline(Request::Start { apps }, Some(START_DEADLINE))
        .await
    {
        let code = ExitCode::from(&err);
        streams.fail(code, &err.to_string());
        let _ = client.request(Request::KillDaemon).await;
        let _ = supervisor.await;
        return code;
    }

    // Whichever branch of the race resolves first assigns `ending`
    // synchronously, before `interrupt` reports `Ready`, so it is set by the
    // time `bleats_with_signal` returns.
    let mut ending: Option<Ending> = None;
    {
        let watcher = watch_until_empty(|| async {
            match client.request(Request::ListFlock).await {
                Ok(Response::Flock(procs)) => sample(&procs),
                // A request error mid-poll is not evidence the flock is gone,
                // and the `supervisor` branch ends the race if it is.
                _ => Sample::Busy,
            }
        });
        tokio::pin!(watcher);

        let interrupt = async {
            tokio::select! {
                reading = &mut watcher => {
                    ending = Some(Ending::Empty(reading));
                }
                result = &mut supervisor => {
                    let failed = !matches!(result, Ok(Ok(())));
                    ending = Some(Ending::SupervisorExited { failed });
                }
            }
        };

        bleats_with_signal(
            &client,
            streams,
            quiet,
            &BleatsArgs {
                selector: "all".to_string(),
                no_follow: false,
                // No backlog: older lines in those files belong to a
                // previous run and would read as this one's output.
                lines: 0,
                err: false,
                out: false,
            },
            interrupt,
        )
        .await;
    }

    // A `JoinHandle` must not be awaited twice, and the `SupervisorExited`
    // case already polled `supervisor` to completion inside the select.
    let already_consumed = matches!(ending, Some(Ending::SupervisorExited { .. }));

    // Stop and delete before asking the shepherd itself to go. Skipped when
    // the supervisor is already gone: `delete_flock_on_shutdown` covers it.
    if tidy_up && !already_consumed {
        let _ = client
            .request(Request::Stop {
                selector: SelectorSpec::All,
            })
            .await;
        let _ = client
            .request(Request::Delete {
                selector: SelectorSpec::All,
            })
            .await;
    }

    // Best-effort: if the supervisor already exited, nothing is listening on
    // the other end and this simply fails.
    let _ = client.request(Request::KillDaemon).await;

    let supervisor_failed = if already_consumed {
        matches!(ending, Some(Ending::SupervisorExited { failed: true }))
    } else {
        !matches!(supervisor.await, Ok(Ok(())))
    };

    // A failed supervisor wins over both of the other outcomes.
    if supervisor_failed {
        return ExitCode::Failure;
    }

    match ending {
        Some(Ending::Empty(Sample::EmptyFailed)) => ExitCode::FlockEmpty,
        Some(Ending::Empty(Sample::EmptyClean)) => ExitCode::Success,
        // `watch_until_empty` never settles on `Busy`. Handled anyway, so
        // nothing here can panic on the way out of a container.
        Some(Ending::Empty(Sample::Busy)) | Some(Ending::SupervisorExited { .. }) | None => {
            ExitCode::Success
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::Format;
    use crate::exit::ExitCode;
    use crate::output::Streams;
    use crate::style;

    fn buffered_streams<'a>(out: &'a mut Vec<u8>, err: &'a mut Vec<u8>) -> Streams<'a> {
        Streams {
            out,
            err,
            style: style::Presentation::BARE,
            fmt: Format::Table,
        }
    }

    /// Drives the guard against a client from a fake shepherd: `run` itself
    /// boots a real supervisor and cannot be handed a scripted version.
    #[tokio::test]
    async fn a_version_skewed_shepherd_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let addr = shep_client::testing::control_address(dir.path());
        let ack = shep_core::protocol::HelloAck {
            daemon_version: "0.1.8".to_string(),
            protocol: shep_core::protocol::PROTOCOL_VERSION,
            pid: 4242,
        };
        let (client, _fake) = shep_client::testing::fake_client_with_ack(&addr, ack).await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = buffered_streams(&mut out, &mut err);
        let code = super::refuse_if_skewed(&mut streams, &client)
            .expect_err("a differing crate version must be refused");
        assert_eq!(code, ExitCode::VersionSkew);
    }

    #[tokio::test]
    async fn a_matching_version_proceeds() {
        let dir = tempfile::tempdir().unwrap();
        let addr = shep_client::testing::control_address(dir.path());
        let ack = shep_core::protocol::HelloAck {
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: shep_core::protocol::PROTOCOL_VERSION,
            pid: 4242,
        };
        let (client, _fake) = shep_client::testing::fake_client_with_ack(&addr, ack).await;

        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut streams = buffered_streams(&mut out, &mut err);
        super::refuse_if_skewed(&mut streams, &client).expect("a matching version is not a skew");
    }
}
