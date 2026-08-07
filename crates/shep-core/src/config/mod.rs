//! Configuration: per-app schema (Flockfile), normalization, discovery,
//! and the daemon's own `shep.toml`

pub mod app;
pub mod daemon;
pub mod flockfile;
pub mod normalize;

pub use app::AppConfig;
pub use daemon::{DaemonConfig, DaemonConfigError};
pub use flockfile::{Flockfile, FlockfileError};
pub use normalize::{ConfigError, ResolvedApp};
