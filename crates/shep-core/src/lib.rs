//! Shared foundation of the shep workspace
//!
//! Typed configuration (Flockfile + daemon config), value newtypes,
//! process selectors, `$SHEP_HOME` paths, and wire protocol version 1.
//! Every other crate depends on this one; it depends on no sibling.
//!
//! # Quick start
//! ```
//! use shep_core::prelude::*;
//!
//! let app: AppConfig = toml::from_str("name = \"web\"\nscript = \"./srv\"").unwrap();
//! assert!(app.autorestart);
//! let limit: MemSize = "512M".parse().unwrap();
//! assert_eq!(limit.bytes(), 512 << 20);
//! ```
//!
//! Module-by-module design: `docs/systematic-refactor/refactor-workspace/map.md`;
//! behavior contract: `docs/specs/shep-v1.md`.

#![doc(test(attr(deny(warnings))))]
#![forbid(unsafe_code)]

// Declared above `barks`, `kv` and `overrides` because all three end a
// write through it, as do `shep.toml`, `dogs.toml` and the muster roll in
// the crates above this one: one definition of what finishes an atomic
// replace, rather than six writers each deciding how far to flush.
pub mod atomic_file;
pub mod barks;
pub mod config;
pub mod kv;
// One definition of the log-line timestamp for the writer and every reader:
// the daemon stamps, and three different file readers in shep-cli strip.
pub mod logstamp;
pub mod overrides;
pub mod paths;
pub mod protocol;
pub mod selector;
pub mod signals;
pub mod status;
// Declared next to `protocol`, deliberately: that module owns what travels
// over the control plane and this one owns what carries it. Keeping the two
// in one crate is what lets every layer above them — the client's actor, the
// daemon's connection state machine, every RPC verb — be written once with
// no `cfg` in it at all.
pub mod transport;
pub mod values;

/// One-import surface for downstream crates
pub mod prelude {
    #[doc(no_inline)]
    pub use crate::config::{AppConfig, DeclaredApp, Flockfile};
    #[doc(no_inline)]
    pub use crate::paths::ShepPaths;
    #[doc(no_inline)]
    pub use crate::selector::ProcessSelector;
    #[doc(no_inline)]
    pub use crate::status::ProcStatus;
    #[doc(no_inline)]
    pub use crate::values::{MemSize, UpDuration};
}
