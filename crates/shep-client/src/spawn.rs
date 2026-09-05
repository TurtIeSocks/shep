//! Autostart: [`connect_or_spawn`], [`connect_or_spawn_with`], [`SpawnError`]
//!
//! A bound-but-not-accepting unix socket still completes `connect(2)` into
//! the kernel backlog, so a bare connect can't tell "nothing is listening"
//! from "something is listening and not yet answering". Every probe here,
//! including the first, is a full handshake via
//! [`Client::connect_with_timeout`]. Only [`ConnectError::Connect`] (the OS
//! refusing the connect outright) reads as "nothing to disturb, launch a
//! daemon"; [`ConnectError::HandshakeTimeout`] on that first probe means a
//! daemon is already bound and mid-boot, so nothing launches and probing continues.

use core::fmt;
use std::path::Path;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use crate::client::Client;
use crate::connection::ConnectError;

/// How long the spawn-and-wait path will keep probing before giving up.
pub const SPAWN_DEADLINE: Duration = Duration::from_secs(30);

/// First retry gap; grows ×1.5 up to [`BACKOFF_CAP`] (spec §6).
pub const BACKOFF_START: Duration = Duration::from_millis(100);

/// Ceiling for the retry gap (spec §6).
pub const BACKOFF_CAP: Duration = Duration::from_secs(5);

/// The per-attempt handshake budget, defined in the (private) `connection`
/// module and surfaced here so the whole spawn budget reads from one place.
pub use crate::connection::HANDSHAKE_TIMEOUT;

/// Exit status a `shep daemon` child uses when another daemon already holds
/// this `$SHEP_HOME` (`shep-cli`'s `ExitCode::DaemonAlreadyRunning`).
///
/// Couples this client to the CLI's exit-code taxonomy: an exit status is
/// the only channel a dead child leaves behind. Changing either side
/// without the other reintroduces the race in [`SpawnError::DaemonExited`].
pub const DAEMON_ALREADY_RUNNING: i32 = 10;

/// Every timing [`connect_or_spawn`] obeys, injectable so tests do not spend
/// 30 wall-clock seconds proving a probe is bounded.
#[derive(Debug, Clone)]
pub struct SpawnOptions {
    /// Total wall-clock budget for the whole spawn-and-wait path, from the
    /// first probe failure to giving up.
    pub deadline: Duration,
    /// The retry gap before the first post-launch probe.
    pub backoff_start: Duration,
    /// Ceiling the retry gap grows toward (×1.5 per attempt) but never past.
    pub backoff_cap: Duration,
    /// Budget for one connect-plus-handshake attempt, passed to
    /// [`Client::connect_with_timeout`] on every probe.
    pub handshake_timeout: Duration,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self {
            deadline: SPAWN_DEADLINE,
            backoff_start: BACKOFF_START,
            backoff_cap: BACKOFF_CAP,
            handshake_timeout: HANDSHAKE_TIMEOUT,
        }
    }
}

/// What [`connect_or_spawn`] (or [`connect_or_spawn_with`]) had to do to hand
/// back a connected [`Client`].
///
/// No `#[non_exhaustive]`: the algorithm has exactly two steps (probe,
/// then launch and retry), and each variant is one step's success exit.
/// Contrast [`SpawnError`], which does carry the attribute, since a
/// failure can originate from either step, the launcher, or the deadline.
#[derive(Debug)]
pub enum SpawnOutcome {
    /// The very first probe completed a handshake: a daemon was already up,
    /// and nothing was launched.
    Connected(Client),
    /// The first probe found nothing (or an unfinished handshake), a daemon
    /// was launched (or was already mid-boot), and a later probe succeeded.
    Spawned(Client),
}

/// Why [`connect_or_spawn`] (or [`connect_or_spawn_with`]) failed to hand
/// back a connected [`Client`].
///
/// Non-exhaustive: expect more variants.
#[derive(Debug)]
#[non_exhaustive]
pub enum SpawnError {
    /// The first probe failed for a reason other than "nothing is
    /// listening" or "listening but not yet answering": a protocol
    /// mismatch, above all. A daemon that refuses on version skew is
    /// still a daemon; launching a second one would fail identically.
    Connect(ConnectError),
    /// The caller-supplied launcher itself returned an `io::Error`:
    /// `Command::spawn()` failed before any daemon process existed to probe.
    Launch(std::io::Error),
    /// The launched child exited with a status other than
    /// [`DAEMON_ALREADY_RUNNING`] before any later probe succeeded. Any
    /// other non-zero (or signalled) exit is treated as fatal rather than
    /// burning the rest of the deadline probing a corpse; an exit carrying
    /// [`DAEMON_ALREADY_RUNNING`] is the losing side of a cold-start race
    /// (another process's `flock(2)` won) and is not reported this way.
    DaemonExited {
        /// The child's own exit status.
        status: ExitStatus,
    },
    /// No probe succeeded before `opts.deadline` elapsed.
    DeadlineExpired {
        /// The budget that was exceeded: `opts.deadline`, not the
        /// wall-clock time actually spent.
        after: Duration,
        /// The most recent probe failure, if any probe was attempted: says
        /// whether nothing was listening, or something never answered.
        last: Option<ConnectError>,
    },
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(err) => write!(f, "{err}"),
            Self::Launch(err) => write!(f, "failed to launch the daemon: {err}"),
            Self::DaemonExited { status } => {
                write!(
                    f,
                    "the daemon process exited before it started answering: {status}"
                )
            }
            Self::DeadlineExpired {
                after,
                last: Some(last),
            } => write!(f, "no daemon answered within {after:?}: {last}"),
            Self::DeadlineExpired { after, last: None } => {
                write!(f, "no daemon answered within {after:?}")
            }
        }
    }
}

impl core::error::Error for SpawnError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Connect(err) => Some(err),
            Self::Launch(err) => Some(err),
            Self::DaemonExited { .. } => None,
            Self::DeadlineExpired { last, .. } => last
                .as_ref()
                .map(|err| err as &(dyn core::error::Error + 'static)),
        }
    }
}

/// Connects to `socket`, launching a daemon via `launch` if nothing answers
/// the first probe.
///
/// Shorthand for [`connect_or_spawn_with`]`(socket, launch,
/// SpawnOptions::default())`. Production code calls this and never names a
/// timing; [`connect_or_spawn_with`] exists so tests can inject a much
/// shorter deadline.
///
/// # Errors
///
/// See [`connect_or_spawn_with`].
pub async fn connect_or_spawn<L>(socket: &Path, launch: L) -> Result<SpawnOutcome, SpawnError>
where
    L: FnOnce() -> std::io::Result<std::process::Child> + Send + 'static,
{
    connect_or_spawn_with(socket, launch, SpawnOptions::default()).await
}

/// As [`connect_or_spawn`], with caller-supplied timing.
///
/// Probes with a full handshake first: [`ConnectError::Connect`] launches `launch` on a blocking thread, since a real launcher's own filesystem calls would stall the runtime if run inline; [`ConnectError::HandshakeTimeout`] means a daemon is already mid-boot, so nothing launches, and the retry loop follows either way.
/// Each loop iteration then checks the launched child, where only [`DAEMON_ALREADY_RUNNING`] is not fatal. Only [`ConnectError::ProtocolMismatch`] propagates immediately as [`SpawnError::Connect`]; every other probe error is retried until `opts.deadline` and can surface later in [`SpawnError::DeadlineExpired::last`].
///
/// # Errors
///
/// - [`SpawnError::Connect`]: the first probe hit anything besides [`ConnectError::Connect`] or [`ConnectError::HandshakeTimeout`], or a later probe hit [`ConnectError::ProtocolMismatch`].
/// - [`SpawnError::Launch`]: `launch` itself returned an `io::Error`.
/// - [`SpawnError::DaemonExited`]: the child exited with any other status before a probe succeeded.
/// - [`SpawnError::DeadlineExpired`]: no probe succeeded before `opts.deadline`.
pub async fn connect_or_spawn_with<L>(
    socket: &Path,
    launch: L,
    opts: SpawnOptions,
) -> Result<SpawnOutcome, SpawnError>
where
    L: FnOnce() -> std::io::Result<std::process::Child> + Send + 'static,
{
    match Client::connect_with_timeout(socket, opts.handshake_timeout).await {
        Ok(client) => return Ok(SpawnOutcome::Connected(client)),
        Err(ConnectError::Connect { .. }) => {}
        Err(err @ ConnectError::HandshakeTimeout { .. }) => {
            return probe_until_ready(socket, None, opts, Some(err)).await;
        }
        Err(other) => return Err(SpawnError::Connect(other)),
    }

    let launched = tokio::task::spawn_blocking(launch).await;
    let child = match launched {
        Ok(result) => result.map_err(SpawnError::Launch)?,
        // A panic inside the launcher must stay a panic. Three tests in this
        // task launch with `unreachable!("...")` as their whole assertion; if
        // a `JoinError` swallowed that, all three would silently stop
        // testing anything.
        Err(join) if join.is_panic() => std::panic::resume_unwind(join.into_panic()),
        Err(join) => return Err(SpawnError::Launch(std::io::Error::other(join))),
    };

    probe_until_ready(socket, Some(child), opts, None).await
}

/// The retry loop shared by both entry points: keep probing `socket` until
/// one attempt completes a handshake or `opts.deadline` elapses.
///
/// `child` is `None` only for the "already bound, don't launch" branch;
/// otherwise it is the process just launched, dropped on every exit path
/// (the caller gets a [`Client`], never the `Child`, and this function
/// only `try_wait()`s it, never `wait()`s).
///
/// `seed` carries the `ConnectError` that entered the no-launch branch, so
/// a `DeadlineExpired` reached from there still names a real failure.
async fn probe_until_ready(
    socket: &Path,
    mut child: Option<std::process::Child>,
    opts: SpawnOptions,
    seed: Option<ConnectError>,
) -> Result<SpawnOutcome, SpawnError> {
    let start = Instant::now();
    let mut backoff = opts.backoff_start;
    let mut last = seed;
    // flips once try_wait() has reaped the child, so later iterations
    // stop calling try_wait() on an already-reaped child, which errors
    let mut child_reaped = false;

    loop {
        if start.elapsed() >= opts.deadline {
            return Err(SpawnError::DeadlineExpired {
                after: opts.deadline,
                last,
            });
        }

        if !child_reaped
            && let Some(proc) = child.as_mut()
            && let Ok(Some(status)) = proc.try_wait()
        {
            child_reaped = true;
            // a clean exit (status 0) is fatal too: `child` is the daemon
            // process itself, so any exit at all means it isn't serving
            if status.code() != Some(DAEMON_ALREADY_RUNNING) {
                return Err(SpawnError::DaemonExited { status });
            }
            // Otherwise: another process won the cold-start race. The
            // daemon it brought up is still worth probing for.
        }

        tokio::time::sleep(backoff).await;
        backoff = backoff.mul_f64(1.5).min(opts.backoff_cap);

        match Client::connect_with_timeout(socket, opts.handshake_timeout).await {
            Ok(client) => return Ok(SpawnOutcome::Spawned(client)),
            // still a daemon: propagate immediately rather than burn the
            // rest of the deadline on a condition already answered
            Err(err @ ConnectError::ProtocolMismatch { .. }) => {
                return Err(SpawnError::Connect(err));
            }
            Err(err) => last = Some(err),
        }
    }
}
