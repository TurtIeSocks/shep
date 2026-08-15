//! `shep serve`'s static-file surface: request-target resolution and the
//! filesystem containment it feeds.
//!
//! Two tiers, split deliberately (Phase 15 decision 5): [`path`] is pure —
//! no `cfg`, no I/O, `&str` in and root-relative segments out — and compiles
//! on every target this workspace ships, Windows included. `fs` is
//! `#[cfg(unix)]` and `async` over `tokio::fs`: it is where the containment
//! walk and the file open live, because both are syscalls and this phase
//! adds tokio's `fs` feature specifically so a request never blocks a
//! runtime worker on a blocking `std::fs` call.
//!
//! `mime` and `listing` are pure like `path`: an extension-to-content-type
//! table and the autoindex HTML renderer, neither touching a filesystem or
//! a `cfg`. `auth` is pure-ish: its own permission-mode check is the one
//! unix-only piece, isolated to a single internal function so the rest —
//! the creds parse and the constant-time basic-auth comparison — stays
//! portable like the others.
//!
//! `worker` is `#[cfg(unix)]`, same as `fs`: it binds a real listener and
//! reads real files, and it is where every one of the modules above is
//! actually called — [`worker::ServeConfig`] and [`worker::run`] are what
//! the verb (Task 7) builds and starts.

mod path;

#[cfg(unix)]
mod fs;

mod mime;

mod listing;

// `pub(crate)`: `commands::serve` (Task 7) is the verb that builds
// `auth::load`'s `Credentials` and `worker::ServeConfig` and calls
// `worker::run` — a sibling module tree under `commands/`, not a descendant
// of this one, so its items need to reach past `serve`'s own privacy
// boundary rather than merely being `pub` inside it.
pub(crate) mod auth;

#[cfg(unix)]
pub(crate) mod worker;
