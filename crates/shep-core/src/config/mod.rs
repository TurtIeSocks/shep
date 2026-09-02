//! Configuration: per-app schema (Flockfile), normalization, discovery,
//! and the daemon's own `shep.toml`

pub mod app;
pub mod apply;
pub mod cron;
pub mod daemon;
pub mod flockfile;
pub mod kill_signal;
pub mod normalize;
pub mod probe;
#[cfg(feature = "schema")]
pub mod scaffold;
pub mod template;

pub use app::{AppConfig, ProbeConfig, ProbeKind};
pub use apply::{ApplyGroup, ResetDepth, apply_group};
pub use cron::{CronParseError, CronSchedule, CronScheduleError};
pub use daemon::{DaemonConfig, DaemonConfigError, DaemonOverrides, LogLevel, parse_daemon_bool};
pub use flockfile::{DeclaredApp, FlockFormat, Flockfile, FlockfileError, discover};
#[cfg(feature = "schema")]
pub use flockfile::{flockfile_schema_json, flockfile_schema_string};
pub use kill_signal::KillSignal;
pub use normalize::{
    NormalizeError, ResolvedApp, TildeError, expand_home_tilde, normalize, normalize_all,
};
pub use probe::{ProbeTarget, ProbeTargetError};
#[cfg(feature = "schema")]
pub use scaffold::{CURATED, Depth, Scaffold, ScaffoldError};
