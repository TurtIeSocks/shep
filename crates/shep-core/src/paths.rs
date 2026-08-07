//! On-disk layout of `$SHEP_HOME`
//!
//! One resolver, no hidden `std::env` reads — the environment comes in as a
//! closure so tests and the daemon share one code path.

use std::path::{Path, PathBuf};

/// Resolved filesystem layout for one shep home
///
/// All paths are derived from `$SHEP_HOME` (default `<home>/.shep`); nothing
/// here touches the filesystem — creation happens daemon-side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShepPaths {
    /// Root: `$SHEP_HOME`
    pub home: PathBuf,
    /// Daemon config: `shep.toml`
    pub daemon_config: PathBuf,
    /// Flock snapshot (muster roll): `flock.json`
    pub snapshot: PathBuf,
    /// Log directory
    pub logs: PathBuf,
    /// Pid-file directory
    pub pids: PathBuf,
    /// Runtime dir (sockets; created 0700)
    pub run: PathBuf,
    /// Control socket: `run/shep.sock`
    pub socket: PathBuf,
    /// Bark history ring: `barks.jsonl`
    pub barks: PathBuf,
}

impl ShepPaths {
    /// Windows named-pipe identity for this home: `\\.\pipe\shep-<sanitized>`
    ///
    /// Derived from the home path (non-alphanumerics become `-`) so distinct
    /// `$SHEP_HOME`s never collide on the global pipe namespace.
    #[must_use]
    pub fn pipe_name(&self) -> String {
        let sanitized: String = self
            .home
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let trimmed = sanitized.trim_matches('-');
        format!(r"\\.\pipe\shep-{trimmed}")
    }

    /// Resolves the layout from an environment lookup and the user's home dir
    #[must_use]
    pub fn resolve(env: &dyn Fn(&str) -> Option<String>, home_dir: &Path) -> Self {
        let home = env("SHEP_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir.join(".shep"));
        let run = home.join("run");
        Self {
            daemon_config: home.join("shep.toml"),
            snapshot: home.join("flock.json"),
            logs: home.join("logs"),
            pids: home.join("pids"),
            socket: run.join("shep.sock"),
            barks: home.join("barks.jsonl"),
            run,
            home,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn default_layout_under_home_dir() {
        let p = ShepPaths::resolve(&no_env, Path::new("/home/rin"));
        assert_eq!(p.home, Path::new("/home/rin/.shep"));
        assert_eq!(p.daemon_config, Path::new("/home/rin/.shep/shep.toml"));
        assert_eq!(p.snapshot, Path::new("/home/rin/.shep/flock.json"));
        assert_eq!(p.logs, Path::new("/home/rin/.shep/logs"));
        assert_eq!(p.pids, Path::new("/home/rin/.shep/pids"));
        assert_eq!(p.run, Path::new("/home/rin/.shep/run"));
        assert_eq!(p.socket, Path::new("/home/rin/.shep/run/shep.sock"));
        assert_eq!(p.barks, Path::new("/home/rin/.shep/barks.jsonl"));
    }

    #[test]
    fn shep_home_env_overrides_root() {
        let env = |key: &str| (key == "SHEP_HOME").then(|| "/srv/shep".to_string());
        let p = ShepPaths::resolve(&env, Path::new("/home/rin"));
        assert_eq!(p.home, Path::new("/srv/shep"));
        assert_eq!(p.socket, Path::new("/srv/shep/run/shep.sock"));
    }

    #[test]
    fn pipe_name_is_per_home_and_sanitized() {
        // Windows transport identity (spec §6): derived from SHEP_HOME so
        // two homes never share a pipe; non-alphanumerics collapse to '-'.
        let p = ShepPaths::resolve(&no_env, Path::new("/home/rin"));
        assert_eq!(p.pipe_name(), r"\\.\pipe\shep-home-rin--shep");
        let env = |key: &str| (key == "SHEP_HOME").then(|| "/srv/shep".to_string());
        let q = ShepPaths::resolve(&env, Path::new("/home/rin"));
        assert_eq!(q.pipe_name(), r"\\.\pipe\shep-srv-shep");
    }
}
