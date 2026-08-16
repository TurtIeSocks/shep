//! Per-verb command implementations, `daemon` first. OS tier: gated
//! `#[cfg(unix)]` at this module's own declaration in `main.rs`, so nothing
//! declared beneath it needs a `cfg` of its own.

pub mod admin;
pub mod bleats;
pub mod daemon;
pub mod dev;
pub mod dogs;
pub(crate) mod empty;
pub(crate) mod foreground;
pub mod import;
pub mod kv;
pub mod lifecycle;
pub mod logs;
pub mod muster;
pub mod query;
pub(crate) mod reap;
pub mod runtime;
pub mod schema;
pub(crate) mod selector;
pub mod serve;
pub(crate) mod shep_toml;
pub mod signal;
pub(crate) mod startup;
pub mod trigger;
pub mod whisper;
