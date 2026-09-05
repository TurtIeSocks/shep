//! `shep serve`'s static-file surface: request-target resolution and the
//! filesystem containment it feeds.
//!
//! [`path`], `mime` and `listing` are pure: no `cfg`, no I/O, and they
//! compile on every target. `fs` and `worker` are `#[cfg(unix)]` and
//! `async` over `tokio::fs`, since the containment walk, the file open
//! and the listener all cross a syscall. `auth` is pure except for its
//! permission-mode check, isolated to one internal function.

mod path;

pub(crate) mod fs;

mod mime;

mod listing;

// `pub(crate)`: `commands::serve`, a sibling module tree, builds
// `auth::load`'s `Credentials` and `worker::ServeConfig` and calls
// `worker::run`.
pub(crate) mod auth;

pub(crate) mod worker;
