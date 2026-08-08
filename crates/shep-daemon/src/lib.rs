//! Supervisor engine for process lifecycle management
//!
//! Owned by the daemon: spawns processes via [`ProcessRunner`](runner::ProcessRunner),
//! orchestrates restart policy, tracks logs and shepherd-channel traffic, and
//! gates lifecycle commands (start, stop, restart, delete, shutdown) through a
//! single [`SupervisorHandle`](supervisor::SupervisorHandle). Pure decision logic
//! (brain, backoff, entry assembly) is IO-free for deterministic testing under a
//! paused tokio clock. The OS tier (real spawning, signal delivery, the kill
//! ladder) is unix-only. Production daemon embeds this crate; the CLI
//! re-executes itself with a hidden `daemon` subcommand to daemonize.
//!
//! ## Engine taxonomy
//!
//! ##### Pure logic
//!
//! - [`brain`]: restart decision tree given exit outcome, uptime, and budget
//! - [`backoff`]: restart delay computation per the spec's exponential backoff rule
//! - [`assemble`]: process env, log paths, and spawn spec assembly
//! - [`entry`]: process lifecycle state, restart budget, reload state machine
//!
//! ##### Abstractions
//!
//! - [`runner`]: [`ProcessRunner`](runner::ProcessRunner) spawn seam with two impls
//! - [`fake`]: deterministic scripted [`ProcessRunner`](runner::ProcessRunner) (test-only, or test-fakes feature)
//!
//! ##### OS tier
//!
//! - [`boot`]: daemon boot — `0700` layout dirs, pidfile, socket bind with stale-socket
//!   recovery, the readiness pipe, signal handlers, and the ordered teardown sequence (unix-only)
//! - [`sys`]: adopting an inherited descriptor — the phase's one `unsafe` block (unix-only)
//! - [`tokio_runner`]: real [`ProcessRunner`](runner::ProcessRunner) over `tokio::process` (unix-only)
//! - [`server`]: the unix-socket connection layer — peer-cred auth, handshake, subscriptions (unix-only)
//!
//! ##### Orchestration
//!
//! - [`supervisor`]: the actor — owns registered entries, spawns per-sheep tasks, routes commands
//! - [`kill`]: kill ladder — SIGTERM, SIGKILL escalation (portable, generic over [`RunningProcess`](runner::RunningProcess))
//! - [`channel`]: shepherd channel codec (child↔daemon messages, newline-JSON)
//! - [`bus`]: the daemon-wide event bus — topic-glob filtering, per-subscriber forwarder tasks
//! - [`snapshot`]: the muster roll — debounced atomic `flock.json` writes, restart-survival restore
//! - [`rpc`]: request dispatch — verb routing onto [`SupervisorHandle`](supervisor::SupervisorHandle), typed errors, per-call deadlines
//!
//! # Quick start
//!
//! This example builds a supervisor engine with a scripted fake runner,
//! registers one app, and lists the live processes.
//! Compile with `--all-features` (the example requires `test-fakes`).
//!
//! ```no_run
//! # #[cfg(feature = "test-fakes")]
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use shep_daemon::fake::{ProcScript, ScriptedRunner};
//! use shep_daemon::supervisor::spawn_supervisor;
//! use shep_core::config::AppConfig;
//! use shep_core::config::normalize;
//! use shep_core::paths::ShepPaths;
//! use std::path::Path;
//!
//! // Create a fake runner that spawns one process never exiting
//! let runner = ScriptedRunner::new(vec![ProcScript::never_exits()]);
//!
//! // Set up temporary paths for this example
//! let paths = ShepPaths::resolve(&|_| None, Path::new("/tmp/shep-example"));
//!
//! // Create a broadcast channel for events
//! let (events, _rx) = tokio::sync::broadcast::channel(64);
//!
//! // Spawn the supervisor actor
//! let handle = spawn_supervisor(runner, paths, events);
//!
//! // Build one app config and normalize it
//! let app = AppConfig::minimal("web", "./server");
//! let resolved = normalize(app)?;
//!
//! // Start the app (creates one instance)
//! let infos = handle.start(vec![resolved]).await?;
//! println!("Started: {} instance(s)", infos.len());
//!
//! // List all registered processes
//! let list = handle.list().await;
//! for info in &list {
//!     println!("  ID {} ({}): {:?}", info.id, info.name, info.status);
//! }
//!
//! // Gracefully shut down all processes
//! handle.shutdown().await;
//!
//! Ok(())
//! # }
//! # #[tokio::main]
//! # async fn main() {
//! #     #[cfg(feature = "test-fakes")]
//! #     example().await.ok();
//! # }
//! ```
//!
//! ## Reference
//!
//! [`ProcessRunner`](runner::ProcessRunner) spawns a child process and returns
//! a [`RunningProcess`](runner::RunningProcess) handle plus a [`ProcIo`](runner::ProcIo)
//! bundle with channels for logs and shepherd messages. The fake runner
//! ([`ScriptedRunner`](fake::ScriptedRunner)) drives deterministic tests;
//! [`spawn_supervisor`](supervisor::spawn_supervisor) wires these together into
//! the core actor loop.

#![doc(test(attr(deny(warnings))))]
#![deny(unsafe_code)]

pub mod assemble;
pub mod backoff;
pub mod brain;
pub mod bus;
pub mod channel;
pub mod entry;
pub mod kill;
pub mod rpc;
pub mod runner;
pub mod snapshot;
pub mod supervisor;

use std::time::SystemTime;

/// The one real-time read shared across this crate: wall-clock milliseconds
/// since the Unix epoch, for [`BusEvent::Process::at_ms`](shep_core::protocol::BusEvent::Process)
/// and [`FlockSnapshot::saved_at_ms`](snapshot::FlockSnapshot::saved_at_ms).
/// Everything else in the engine uses the paused-clock-aware
/// `tokio::time::Instant`.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

// IR-33: one crate-root fixture module. Every test mod from Task 3 onward
// (and the harness in Tasks 4-5) shares this one `test_paths` helper instead
// of hand-rolling its own.
#[cfg(test)]
pub(crate) mod testing {
    use std::sync::Arc;

    use shep_core::paths::ShepPaths;
    use tokio::sync::{broadcast, watch};

    use crate::fake::{ProcScript, ScriptedRunner};
    use crate::rpc::RpcContext;
    use crate::snapshot::FlockRegistry;
    use crate::supervisor::spawn_supervisor;

    // WHY a shallow home: later tasks bind a UDS under `run/`, and sun_path
    // caps a socket path near 104 bytes. Using the tempdir root as
    // $SHEP_HOME (no extra nesting) keeps every test in this crate under the
    // limit on macOS, whose temp paths are already long.
    pub(crate) fn test_paths(dir: &tempfile::TempDir) -> ShepPaths {
        let home = dir.path().to_path_buf();
        ShepPaths::resolve(
            &|key| (key == "SHEP_HOME").then(|| home.display().to_string()),
            std::path::Path::new("/nonexistent"),
        )
    }

    // IR-33: `rpc.rs`'s dispatch tests (Task 4) and the connection-server's
    // tests (Task 5) need the exact same fixture — one factory, not two.
    pub(crate) struct Harness {
        pub(crate) ctx: RpcContext,
        // Kept alive only: dropping the tempdir would remove the paths `ctx`
        // still points at.
        _dir: tempfile::TempDir,
        // Kept alive only: dropping the sender's last receiver would turn
        // every future `events.send()` into a silent no-op.
        _events_rx: broadcast::Receiver<shep_core::protocol::BusEvent>,
        pub(crate) shutdown_rx: watch::Receiver<bool>,
    }

    /// Builds one supervisor engine (a [`ScriptedRunner`] replaying `scripts`)
    /// plus a fresh [`RpcContext`] wired to it.
    pub(crate) fn harness(scripts: Vec<ProcScript>) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let (events, events_rx) = broadcast::channel(256);
        let supervisor =
            spawn_supervisor(ScriptedRunner::new(scripts), paths.clone(), events.clone());
        let (shutdown, shutdown_rx) = watch::channel(false);
        Harness {
            ctx: RpcContext {
                supervisor,
                events,
                registry: FlockRegistry::new(),
                snapshot_path: paths.snapshot.clone(),
                daemon_version: "0.1.0".to_string(),
                pid: 4242,
                shutdown: Arc::new(shutdown),
            },
            _dir: dir,
            _events_rx: events_rx,
            shutdown_rx,
        }
    }
}

// Unix-only: built on `std::os::unix::fs::PermissionsExt` (directory mode
// bits) and `tokio::net::UnixListener`. Doc lives inside boot.rs's own
// `//!` header (not here) — see the comment on `server` below for why an
// outer `///` here would be the wrong place for it.
#[cfg(unix)]
pub mod boot;

// Unix-only: `std::os::unix::io::{FromRawFd, RawFd}` and the one `unsafe` block
// in this crate (adopting the readiness pipe's inherited descriptor) have no
// portable equivalent. Doc lives inside sys.rs's own `//!` header (IR-24's
// rationale essay), not here — same reasoning as `server`'s note below.
#[cfg(unix)]
pub mod sys;

/// Real [`ProcessRunner`](runner::ProcessRunner) over actual OS processes.
///
/// Unix-only: it's built on `nix` (process-group signals) and `command-fds`
/// (fd-3 passing), both `#[cfg(unix)]` deps (see this crate's `Cargo.toml`).
/// The pure tier above (types, traits, the scripted fake) compiles on every
/// platform; only this OS tier is gated out on Windows.
#[cfg(unix)]
pub mod tokio_runner;

// Unix-only for the same reason as `tokio_runner` above: it is built on
// `tokio::net`'s unix-socket types. Task 4's `rpc` dispatcher (and
// everything it calls) stays portable; this module is the thing that
// actually opens a socket. Doc lives inside server.rs's own `//!` header
// (not here) — an outer `///` doc on this declaration would merge with
// that inner doc and rustdoc would resolve the WHOLE merged block's
// intra-doc links against this file's scope, breaking every bare
// same-module link inside server.rs's own header (confirmed by a minimal
// repro during Task 5's docs gate).
#[cfg(unix)]
pub mod server;

/// Deterministic scripted [`ProcessRunner`](runner::ProcessRunner), reused by
/// this crate's own tests and (behind `test-fakes`) by Phase 2b's tests.
#[cfg(any(test, feature = "test-fakes"))]
pub mod fake;
