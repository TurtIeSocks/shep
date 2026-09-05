//! Per-verb command implementations, `daemon` first.
//!
//! `mod commands;` at `lib.rs` carries no platform gate: every module here
//! compiles on every platform, and a Unix call site gates itself.

pub mod admin;
pub mod bleats;
pub(crate) mod bounded;
pub mod daemon;
pub mod dev;
pub(crate) mod dog_migration;
pub mod dogs;
pub(crate) mod empty;
pub(crate) mod foreground;
pub mod import;
pub(crate) mod init;
pub mod kv;
pub mod lifecycle;
pub mod logs;
pub mod muster;
pub mod query;
// `shep runtime`'s PID-1 zombie reaper. Windows has no zombie state and no
// reparent-to-init rule, so there is nothing for a reaper to do.
#[cfg(unix)]
pub(crate) mod reap;
pub mod runtime;
pub mod schema;
pub(crate) mod selector;
pub mod serve;
pub(crate) mod settings;
pub(crate) mod shep_toml;
pub mod signal;
// `shep startup` installs a boot-time unit: systemd, launchd, openrc, or a
// BSD `rc.d` script. Windows' equivalent is a Service Control Manager
// service, a different program shape; the verbs refuse there.
#[cfg(unix)]
pub(crate) mod startup;
pub mod trigger;
pub mod whisper;
