//! Configuration: per-app schema (Flockfile), normalization, discovery,
//! and the daemon's own `shep.toml`

pub mod app;
pub mod normalize;

pub use app::AppConfig;
pub use normalize::{ConfigError, ResolvedApp};
