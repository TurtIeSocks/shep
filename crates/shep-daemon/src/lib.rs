//! The supervisor engine: process registry actor, spawn/kill/reload state
//! machines, file watcher, background workers, RPC server, event bus,
//! Prometheus metrics, and webhook alerting.
//!
//! Library only — the daemon runs embedded in the `shep` binary (the CLI
//! re-executes itself with a hidden `daemon` subcommand to daemonize).
//! Module-by-module design: `docs/systematic-refactor/refactor-workspace/map.md`.

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
