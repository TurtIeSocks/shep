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

pub mod config;
pub mod paths;
pub mod protocol;
pub mod selector;
pub mod status;
pub mod values;

/// One-import surface for downstream crates
pub mod prelude {
    #[doc(no_inline)]
    pub use crate::config::{AppConfig, Flockfile};
    #[doc(no_inline)]
    pub use crate::paths::ShepPaths;
    #[doc(no_inline)]
    pub use crate::selector::ProcessSelector;
    #[doc(no_inline)]
    pub use crate::status::ProcStatus;
    #[doc(no_inline)]
    pub use crate::values::{MemSize, UpDuration};
}
