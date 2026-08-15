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
//! a `cfg`.

mod path;

#[cfg(unix)]
mod fs;

mod mime;

mod listing;
