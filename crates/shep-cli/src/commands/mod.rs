//! Per-verb command implementations, `daemon` first. OS tier: gated
//! `#[cfg(unix)]` at this module's own declaration in `main.rs`, so nothing
//! declared beneath it needs a `cfg` of its own.

pub mod admin;
pub mod bleats;
pub mod daemon;
pub mod lifecycle;
pub mod logs;
pub mod muster;
pub mod query;
pub(crate) mod selector;
pub mod trigger;
