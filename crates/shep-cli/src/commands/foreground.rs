//! The foreground engine shared by `runtime` and `dev`: boots a shepherd in
//! this process, starts a flock, streams its bleats to stdout, and returns
//! once nothing is online or a signal ends the supervisor.
//!
//! Read decision 12 in this phase's plan before touching this file — the two
//! callers share this engine because the spec describes their common
//! behaviour in the same two words, "foreground" and "auto-exit", and there
//! are exactly two of them.

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
    /// not — the container is going away and a delete would only slow the
    /// shutdown down.
    pub tidy_up: bool,
}

/// How the race between the empty-flock watcher and the supervisor's own
/// task ended, captured while [`bleats_with_signal`] is still running so it
/// can be read back once that call returns.
enum Ending {
    /// [`watch_until_empty`] settled on a debounced reading — the ordinary
    /// case, and the only one [`run`]'s own e2e test drives.
    Empty(Sample),
    /// The supervisor task returned on its own, before the flock ever
    /// finished debouncing empty — a signal `shep_daemon::boot` handled
    /// itself (SIGINT/SIGTERM), ending `RunningDaemon::run`. `failed` is
    /// `true` when that return was `Err`, or the task panicked.
    SupervisorExited { failed: bool },
}

/// Boots a shepherd in this process, starts `options.apps`, streams their
/// bleats to `streams.out`, and returns when nothing is online or a signal
/// arrives.
///
/// The shepherd is reached **over its own socket**, not through
/// `RunningDaemon::context()`, whose own doc reserves that handle for
/// `tests/daemon_e2e.rs`. Three things come out of that: `shep flock` from a
/// second terminal (or `docker exec`) works while this is running, which is
/// most of the reason this mode exists; the start path is the one
/// `shep start` already covers; and shutdown is `Request::KillDaemon`, the
/// message `shep kill` sends.
///
/// Signals need no handler here. `shep_daemon::boot` installs SIGINT and
/// SIGTERM handlers before this function ever has a client, and they run the
/// flock's stop ladder and end `run()`. Installing a second set here would
/// race the first; instead the interrupt passed to `bleats_with_signal`
/// completes when **either** the supervisor task finishes **or** the flock
/// has been empty for three polls (`commands::empty::STRIKES`).
///
/// **Streams are unlocked** — `streams` is expected to hold plain, unlocked
/// handles, never a `StdoutLock`/`StderrLock`. This runs until the flock
/// empties, and a lock held across that wedges the first record the
/// supervisor's own logging writes from a tokio worker thread, in this same
/// process. `daemon`, `bleats` and `lookout` are the existing three verbs
/// this applies to; `runtime` and `dev` make five.
/// Refuses `client` if its shepherd disagrees with this binary's crate
/// version — the guard [`crate::refuse_version_skew`] applies at the three
/// seams in `lib.rs`, reused here directly because [`run`] connects on its
/// own rather than through one of them.
///
/// # Errors
/// [`ExitCode::VersionSkew`], as [`crate::refuse_version_skew`].
fn refuse_if_skewed(streams: &mut Streams<'_>, client: &Client) -> Result<(), ExitCode> {
    crate::refuse_version_skew(streams, client, crate::VersionGuard::Enforce)
}

pub async fn run(streams: &mut Streams<'_>, quiet: bool, options: ForegroundOptions) -> ExitCode {
    let ForegroundOptions {
        paths,
        apps,
        tidy_up,
    } = options;

    // `no_restore: true` unconditionally — a container and a dev session
    // both start from their Flockfile, never from a roll somebody saved on
    // this machine last week (decision 12's own table).
    // `foreground: true` — this process behaves like an init-supervised
    // daemon for readiness-reporting purposes, the same as `shep daemon
    // --foreground`, even though nothing here speaks the notify protocol.
    let daemon_args = DaemonArgs {
        cmd: None,
        no_restore: true,
        foreground: true,
        log_json: None,
        log_level: None,
        socket: None,
        max_cron_sleep: None,
    };

    // `tidy_up` doubles as `BootOptions::delete_flock_on_shutdown` — a
    // session that promises to stop-and-delete its own flock on the way out
    // must keep that promise even when it ends by signal, which reaches
    // `RunningDaemon::run`'s own teardown directly and never runs the
    // `Stop`/`Delete` pair below at all (see that field's own doc).
    let daemon = match boot_supervisor(paths.clone(), &daemon_args, tidy_up).await {
        Ok(daemon) => daemon,
        Err(err) => {
            // `daemon_exit_code` already maps `BootError::AlreadyRunning` to
            // `ExitCode::DaemonAlreadyRunning` (10) — the right answer here
            // too: two `shep runtime` in one container, or a `shep dev`
            // while another is up, is exactly "another shepherd already
            // holds this `$SHEP_HOME`".
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
            // booted or found already up — the opposite of a way out of a
            // skew — so it can never be one of `RECOVERY_VERBS`.
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

    // Races the debounced empty-flock watcher against the supervisor's own
    // task, so `bleats_with_signal` stops streaming the moment either one
    // has something to say. `ending` is set from inside the race itself —
    // whichever branch resolves first runs synchronously up to the
    // assignment before the `interrupt` future ever reports `Ready`, so it
    // is always populated by the time `bleats_with_signal` can have used
    // that branch to return.
    let mut ending: Option<Ending> = None;
    {
        let watcher = watch_until_empty(|| async {
            match client.request(Request::ListFlock).await {
                Ok(Response::Flock(procs)) => sample(&procs),
                // A request error mid-poll (a lagging daemon, a transient
                // wire hiccup) is not evidence the flock is gone — treat it
                // as busy so a blip cannot end the container early. If the
                // daemon is really gone, the `supervisor` branch below is
                // the one that ends the race.
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
                // No backlog: this process started the flock a moment ago,
                // so any older lines in those files belong to a previous
                // run and replaying them here would read as this run's
                // output.
                lines: 0,
                err: false,
                out: false,
            },
            interrupt,
        )
        .await;
    }

    // `ending` names whether the race already consumed `supervisor` (the
    // `SupervisorExited` case: its `JoinHandle` was polled to completion
    // inside the select above, and polling it again would be a bug — a
    // `JoinHandle` must not be awaited twice). Every other case leaves
    // `supervisor` still pending, safe to await for the first time below.
    let already_consumed = matches!(ending, Some(Ending::SupervisorExited { .. }));

    // Stop and delete the flock before asking the shepherd itself to go —
    // `dev`'s own teardown (decision 12's table); `runtime` always passes
    // `tidy_up: false`, so this only ever runs for `dev`. Skipped when the
    // supervisor is already gone: there is nothing left to ask — and when
    // that is how this session ends, `BootOptions::delete_flock_on_shutdown`
    // (set from this same `tidy_up`) is what still keeps the promise, since
    // `RunningDaemon::run`'s own teardown gets there first.
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

    // Best-effort: if the supervisor already exited on its own, nothing is
    // listening on the other end and this simply fails, which is fine —
    // there is nothing left to tell.
    let _ = client.request(Request::KillDaemon).await;

    let supervisor_failed = if already_consumed {
        matches!(ending, Some(Ending::SupervisorExited { failed: true }))
    } else {
        !matches!(supervisor.await, Ok(Ok(())))
    };

    // A supervisor task that returned `Err` (or panicked) wins over both of
    // the other outcomes — decision 13's own ordering.
    if supervisor_failed {
        return ExitCode::Failure;
    }

    match ending {
        Some(Ending::Empty(Sample::EmptyFailed)) => ExitCode::FlockEmpty,
        Some(Ending::Empty(Sample::EmptyClean)) => ExitCode::Success,
        // `watch_until_empty` never settles on `Busy` by construction (its
        // own doc): the debounce only returns once `STRIKES` consecutive
        // non-`Busy` readings were seen. Handled rather than `unreachable!`
        // so a contract violation elsewhere reports a clean exit instead of
        // panicking on the way out of a container.
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

    /// `run`'s own connect (:118) guards the shepherd it just booted, same
    /// as the seams in `lib.rs` guard every other verb — `foreground`
    /// registers a sheep with that shepherd, so it can never be exempt.
    /// This drives the guard directly against a client from a fake
    /// shepherd, which is what Task 5 found actually works; `run` itself
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

    /// A shepherd of this binary's own version is not a skew.
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
