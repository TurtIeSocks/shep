//! Spawn seam between the daemon engine and the OS
//!
//! [`ProcessRunner`] spawns a child; the [`RunningProcess`] it returns owns
//! that one live child for its whole lifetime (pid, wait, signal, kill).
//! Spawn also hands back a [`ProcIo`] bundle of channels so the owning sheep
//! task can pump stdout/stderr lines and shepherd-channel messages without
//! the runner itself blocking on delivery.
//!
//! Two implementations exist: a deterministic scripted fake (this crate's
//! test-only `fake` module, `ScriptedRunner`) for engine tests, and a real
//! runner over OS processes (a later task) for production.

use core::fmt;
use std::collections::BTreeMap;
use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::channel::{ChildMessage, ShepherdMessage};
use crate::privilege::Credentials;

/// One exit observation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitOutcome {
    /// Exit code on normal exit
    pub code: Option<i32>,
    /// Raw unix signal number when killed (`SIGTERM`=15, `SIGKILL`=9, ...)
    pub signal: Option<i32>,
}

/// Typed stop signal
///
/// [`StopSignal::as_raw`] gives the unix number so fake and real runners
/// record identical [`ExitOutcome`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopSignal {
    /// `SIGTERM` — graceful stop request
    Term,
    /// `SIGINT` — interrupt
    Int,
    /// `SIGQUIT` — quit, core-dumping by default
    Quit,
    /// `SIGUSR2` — user-defined signal 2
    Usr2,
    /// `SIGKILL` — unblockable, immediate
    Kill,
}

impl StopSignal {
    /// The raw unix signal number
    #[must_use]
    pub fn as_raw(self) -> i32 {
        match self {
            Self::Term => 15,
            Self::Int => 2,
            Self::Quit => 3,
            Self::Usr2 => 12,
            Self::Kill => 9,
        }
    }
}

/// One stdout/stderr line from a child
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// True = stderr, false = stdout
    pub err: bool,
    /// The line, no trailing newline
    pub line: String,
}

/// IO endpoints handed back by spawn — the runner pumps internally.
///
/// The sheep task owns this and MUST drain every receiver: an undrained
/// `from_child` back-pressures a metric-emitting child until it stalls on
/// its own fd-3 write (see the supervisor's `select!` model, a later task).
#[derive(Debug)]
pub struct ProcIo {
    /// stdout+stderr lines
    pub logs: mpsc::Receiver<LogLine>,
    /// Parsed child→daemon shepherd-channel messages
    pub from_child: mpsc::Receiver<ChildMessage>,
    /// daemon→child shepherd-channel sender
    pub to_child: mpsc::Sender<ShepherdMessage>,
}

/// A live child.
pub trait RunningProcess: Send + 'static {
    /// The OS process id
    fn pid(&self) -> u32;

    /// Resolves exactly once with the exit outcome
    ///
    /// # Cancellation safety
    ///
    /// Dropping the returned future and calling `wait` again is safe: it
    /// neither restarts the wait nor loses whatever progress was already
    /// made toward it. [`tokio::process::Child::wait`] documents this
    /// guarantee for real children; the scripted fake mirrors it by fixing
    /// its exit deadline once, at spawn time, rather than recomputing it on
    /// each `wait` call.
    ///
    /// The future is also explicitly `Send` (RPITIT) because the sheep task
    /// that owns the proc is `tokio::spawn`'ed.
    fn wait(&mut self) -> impl core::future::Future<Output = ExitOutcome> + Send;

    /// Sends a signal to the sheep's whole process group
    ///
    /// Group-wide, not leader-only. A sheep that forks a child without
    /// `exec`ing it — the shape every `thing & wait` wrapper script produces —
    /// keeps that child in its own process group, so a leader-only signal
    /// stops the wrapper and leaves the child running, orphaned and untracked.
    /// Signalling the group instead delivers the graceful stop to the lambs
    /// too, and gives them the same chance to shut down cleanly that the
    /// sheep gets, rather than only ever meeting [`Self::kill_tree`]'s
    /// `SIGKILL`.
    ///
    /// # Errors
    ///
    /// - [`RunnerError::SignalFailed`] — delivery failed (already reaped, `EPERM`).
    ///
    /// # Process-group assumption
    ///
    /// Implementors must spawn each child as the leader of a fresh process
    /// group of its own; this method and [`Self::kill_tree`] both address
    /// that group by the pid [`Self::pid`] reports. A child that escapes its
    /// group after the fact (the `setsid`-in-a-fork daemonize dance) is
    /// beyond the reach of either. The real runner's own `signal_group`
    /// (`tokio_runner.rs`, unix-only, so deliberately not linked from this
    /// portable tier) documents how it establishes the property and what an
    /// implementation that dropped it would do instead.
    fn signal(&mut self, sig: StopSignal) -> Result<(), RunnerError>;

    /// SIGKILLs the whole process group/tree
    ///
    /// The escalation rung above [`Self::signal`]: same group, same
    /// process-group assumption (documented there), but a signal nothing can
    /// catch or ignore.
    ///
    /// # Errors
    ///
    /// - [`RunnerError::SignalFailed`] — delivery failed (already reaped, `EPERM`).
    fn kill_tree(&mut self) -> Result<(), RunnerError>;
}

/// Spawn seam between engine and OS
pub trait ProcessRunner: Send + Sync + 'static {
    /// The live-child type this runner produces
    type Proc: RunningProcess;

    /// Spawns per the spec, returning the proc + its IO bundle
    ///
    /// Must be called from within a Tokio runtime context: both
    /// implementations (the scripted fake and the real runner) spawn
    /// background tasks internally to pump IO.
    ///
    /// # Errors
    ///
    /// - [`RunnerError::SpawnFailed`] — exec failure, permissions, missing binary.
    fn spawn(&self, spec: &SpawnSpec) -> Result<(Self::Proc, ProcIo), RunnerError>;
}

/// Everything a spawn needs, pre-assembled by the assembler (a later task)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    /// Sheep name (for logging/tracing, not passed to the child)
    pub name: String,
    /// Executable path or name (resolved via `PATH` if bare)
    pub program: String,
    /// Argument vector, `argv[1..]`
    pub args: Vec<String>,
    /// Working directory; `None` inherits the daemon's
    pub cwd: Option<PathBuf>,
    /// Environment variables, fully resolved (no daemon-env leakage beyond this map)
    pub env: BTreeMap<String, String>,
    /// File stdout is appended to
    pub out_file: PathBuf,
    /// File stderr is appended to
    pub err_file: PathBuf,
    /// Open the shepherd channel (fd 3 socketpair)
    pub channel: bool,
    /// Unix uid/gid to drop to before exec (`None` inherits the daemon's own
    /// identity). Resolved once per `Start` by [`crate::privilege::resolve`];
    /// see that module for how `user`/`group` config names become this.
    pub credentials: Option<Credentials>,
}

/// Error type returned from spawn and process control
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerError {
    /// The OS refused the spawn (exec failure, permissions, missing binary)
    SpawnFailed(String),
    /// Signal delivery failed (already reaped, `EPERM`)
    SignalFailed(String),
}

impl fmt::Display for RunnerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SpawnFailed(msg) => write!(f, "process spawn failed: {msg}"),
            Self::SignalFailed(msg) => write!(f, "signal delivery failed: {msg}"),
        }
    }
}

impl core::error::Error for RunnerError {}
