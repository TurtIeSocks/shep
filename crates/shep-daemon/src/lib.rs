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
//! - [`tokio_runner`]: real [`ProcessRunner`](runner::ProcessRunner) over `tokio::process` (unix-only)
//! - [`kill`]: kill ladder — SIGTERM, SIGKILL escalation with a timeout
//!
//! ##### Orchestration
//!
//! - [`supervisor`]: the actor — owns registered entries, spawns per-sheep tasks, routes commands
//! - [`channel`]: shepherd channel codec (child↔daemon messages, newline-JSON)
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
//! let (reply, rx) = tokio::sync::oneshot::channel();
//! handle.shutdown(reply).await?;
//! rx.await.ok();
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
pub mod channel;
pub mod entry;
pub mod kill;
pub mod runner;
pub mod supervisor;

/// Real [`ProcessRunner`](runner::ProcessRunner) over actual OS processes.
///
/// Unix-only: it's built on `nix` (process-group signals) and `command-fds`
/// (fd-3 passing), both `#[cfg(unix)]` deps (see this crate's `Cargo.toml`).
/// The pure tier above (types, traits, the scripted fake) compiles on every
/// platform; only this OS tier is gated out on Windows.
#[cfg(unix)]
pub mod tokio_runner;

/// Deterministic scripted [`ProcessRunner`](runner::ProcessRunner), reused by
/// this crate's own tests and (behind `test-fakes`) by Phase 2b's tests.
#[cfg(any(test, feature = "test-fakes"))]
pub mod fake;
