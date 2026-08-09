//! The filesystem-watch subsystem (spec §4).
//!
//! [`source`] is the OS seam: it bridges notify's debounced filesystem
//! events onto a tokio channel.
//!
//! ## Reference
//!
//! - [`source::WatchSource`], [`source::watch_tree`], [`source::WatchError`]

pub mod source;
