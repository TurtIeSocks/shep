//! The supervisor engine: process registry actor, spawn/kill/reload state
//! machines, file watcher, background workers, RPC server, event bus,
//! Prometheus metrics, and webhook alerting.
//!
//! Library only — the daemon runs embedded in the `shep` binary (the CLI
//! re-executes itself with a hidden `daemon` subcommand to daemonize).
//! Module-by-module design: `docs/systematic-refactor/refactor-workspace/map.md`.
