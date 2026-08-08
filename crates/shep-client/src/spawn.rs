//! Autostart: [`connect_or_spawn`], [`connect_or_spawn_with`], [`SpawnError`]
//!
//! A bound-but-not-accepting unix socket still completes `connect(2)` into
//! the kernel backlog (see `connection`'s own doc), so a bare connect can
//! never tell "nothing is listening" apart from "something is listening and
//! not yet answering". [`connect_or_spawn_with`] therefore never treats a
//! plain `connect` success as readiness — every probe it makes, including
//! the very first one, is a full `Connection::open` handshake (private to
//! this crate — see the `connection` module's own doc). Only
//! [`ConnectError::Connect`] (the OS refusing the connect
//! outright — nothing bound at all) is read as "nothing to disturb, launch a
//! daemon"; [`ConnectError::HandshakeTimeout`] on that first probe means a
//! daemon is already bound and mid-boot, so this launches nothing and just
//! keeps probing.

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
/// This couples the client to the CLI's exit-code taxonomy, deliberately:
/// the client cannot inspect a `BootError` across a process boundary, and an
/// exit status is the only channel a dead child leaves behind. Changing
/// either side without the other reintroduces the race documented on
/// [`SpawnError::DaemonExited`].
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
/// Growth is expected — library-crate public error type (IR-20).
#[derive(Debug)]
#[non_exhaustive]
pub enum SpawnError {
    /// The first probe failed for a reason other than "nothing is
    /// listening" or "listening but not yet answering" — a protocol
    /// mismatch, above all. A daemon that refuses the handshake on version
    /// skew is still a daemon; launching a second one would be wrong and
    /// would fail identically.
    Connect(ConnectError),
    /// The caller-supplied launcher itself returned an `io::Error` — the
    /// `Command::spawn()` call failed, before any daemon process existed to
    /// probe.
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
        /// The budget that was exceeded — `opts.deadline`, the value the
        /// caller configured, not the wall-clock time actually spent. The
        /// two differ by however far past the deadline the last attempt
        /// ran.
        after: Duration,
        /// The most recent probe failure, if any probe was ever attempted.
        /// A bare "timed out" tells a caller nothing about why — whether
        /// nothing was ever listening, or something was listening and never
        /// answered, is the whole diagnosis.
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
/// See [`connect_or_spawn_with`] — every error variant it can return, this
/// returns unchanged.
pub async fn connect_or_spawn<L>(socket: &Path, launch: L) -> Result<SpawnOutcome, SpawnError>
where
    L: FnOnce() -> std::io::Result<std::process::Child> + Send + 'static,
{
    connect_or_spawn_with(socket, launch, SpawnOptions::default()).await
}

/// As [`connect_or_spawn`], with caller-supplied timing.
///
/// The contract, in order:
/// 1. Probe `socket` with a full handshake
///    ([`Client::connect_with_timeout`], bounded by
///    `opts.handshake_timeout`). Success hands back
///    [`SpawnOutcome::Connected`] without ever calling `launch`.
/// 2. On [`ConnectError::Connect`] — and only that — call `launch` (on a
///    blocking thread; see below). Nothing is listening, so nothing is there
///    to disturb.
/// 3. On [`ConnectError::HandshakeTimeout`] from that *first* probe, do
///    **not** launch: something is already bound and not yet answering, a
///    daemon caught between binding its socket and starting to accept.
///    Enter the probe loop against the same deadline without spawning a
///    second daemon.
/// 4. Any other [`ConnectError`] propagates immediately as
///    [`SpawnError::Connect`] — see that variant's own doc.
/// 5. Loop until `opts.deadline`: check the launched child (if any), sleep
///    the current backoff, then attempt a full handshake with
///    `opts.handshake_timeout`. Success hands back [`SpawnOutcome::Spawned`].
///    The most recent [`ConnectError`] is kept for
///    [`SpawnError::DeadlineExpired`].
/// 6. Between attempts, the launched child (if any) is checked with
///    `try_wait()`. An exit carrying [`DAEMON_ALREADY_RUNNING`] is not a
///    failure — another process won the cold-start race, so probing
///    continues. Any other exit is fatal: probing stops immediately and
///    [`SpawnError::DaemonExited`] is returned rather than spending the rest
///    of the deadline on a corpse.
///
/// `launch` runs on a blocking thread ([`tokio::task::spawn_blocking`]),
/// never on the async runtime worker: a real launcher (`shep-cli`'s
/// `launch_daemon`) does a directory create, file creates and a
/// `Command::spawn` — several blocking syscalls with no bound — and running
/// those on the executor would stall every other task in the process for as
/// long as the filesystem takes.
///
/// # Errors
///
/// - [`SpawnError::Connect`] — the first probe failed for a reason other
///   than "nothing listening" or "listening but not yet answering".
/// - [`SpawnError::Launch`] — `launch` itself returned an `io::Error`.
/// - [`SpawnError::DaemonExited`] — the launched child exited with a status
///   other than [`DAEMON_ALREADY_RUNNING`] before any probe succeeded.
/// - [`SpawnError::DeadlineExpired`] — no probe succeeded before
///   `opts.deadline`.
///
/// Not a `# Panics` section, deliberately: this function contains no
/// panicking call of its own. But note that if `launch` itself panics, that
/// panic is resumed rather than converted into a `SpawnError` — a caller
/// that deliberately panics inside a test launcher (as several tests in
/// this crate do, to assert that a code path must never call it) needs that
/// panic to still fail the test, not be swallowed by the `JoinHandle`
/// `spawn_blocking` returns it through.
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
/// `child` is `None` only for the "already bound, don't launch" branch
/// ([`ConnectError::HandshakeTimeout`] on the very first probe); otherwise
/// it is the process `connect_or_spawn_with` just launched, owned here for
/// the rest of the call and dropped on every exit path (including the
/// success path — the caller gets a [`Client`], not the `Child`; in
/// production that `Child` *is* the daemon, and this function never
/// `wait()`s it, only `try_wait()`s).
///
/// `seed` carries the `ConnectError` that caused the loop to be entered in
/// the no-launch branch, so a `DeadlineExpired` reached in that branch still
/// names a real probe failure instead of `None`.
async fn probe_until_ready(
    socket: &Path,
    mut child: Option<std::process::Child>,
    opts: SpawnOptions,
    seed: Option<ConnectError>,
) -> Result<SpawnOutcome, SpawnError> {
    let start = Instant::now();
    let mut backoff = opts.backoff_start;
    let mut last = seed;
    // Flips once `try_wait()` has reaped the child — a normal exit, or the
    // `DAEMON_ALREADY_RUNNING` race — so later iterations stop calling
    // `try_wait()` on an already-reaped child, which errors.
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
            Err(err) => last = Some(err),
        }
    }
}
