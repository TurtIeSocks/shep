//! The supervisor engine: process registry actor, spawn/kill/reload state
//! machines, file watcher, background workers, RPC server, event bus,
//! Prometheus metrics, and webhook alerting.
//!
//! Library only — the daemon runs embedded in the `shep` binary (the CLI
//! re-executes itself with a hidden `daemon` subcommand to daemonize).
//! Module-by-module design: `docs/systematic-refactor/refactor-workspace/map.md`.

#![doc(test(attr(deny(warnings))))]
#![deny(unsafe_code)]

pub mod backoff;
pub mod brain;
pub mod channel;
pub mod entry;
pub mod runner;

/// Deterministic scripted [`ProcessRunner`](runner::ProcessRunner), reused by
/// this crate's own tests and (behind `test-fakes`) by Phase 2b's tests.
#[cfg(any(test, feature = "test-fakes"))]
pub mod fake;
