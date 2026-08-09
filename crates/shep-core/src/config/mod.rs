//! Configuration: per-app schema (Flockfile), normalization, discovery,
//! and the daemon's own `shep.toml`

pub mod app;
pub mod cron;
pub mod daemon;
pub mod flockfile;
pub mod normalize;

pub use app::{AppConfig, ProbeConfig, ProbeKind};
pub use cron::{CronParseError, CronSchedule, CronScheduleError};
pub use daemon::{DaemonConfig, DaemonConfigError};
pub use flockfile::{FlockFormat, Flockfile, FlockfileError, discover};
pub use normalize::{NormalizeError, ResolvedApp, normalize, normalize_all};
