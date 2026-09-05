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

// Shared atomic-write primitive: barks, kv, overrides, shep.toml, dogs.toml
// and the muster roll all write through it.
pub mod atomic_file;
pub mod barks;
pub mod config;
// The lock a config file's writers hold across their read-modify-write, and
// the staging file they write through. Lives here rather than in shep-cli
// (where it was born) so shep-daemon can hold it too, once it starts writing
// `dogs.toml`.
pub mod config_lock;
// The probe contract both sides of a dog's `--version`/`--schema` answer
// parse: flag names, the answer grammar, the schema's secret marker key.
pub mod dogs;
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
// OS-specific transport (unix socket or named pipe) lives here so nothing
// above it needs a `cfg`.
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
