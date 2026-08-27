//! On-disk layout of `$SHEP_HOME`
//!
//! One resolver, no hidden `std::env` reads — the environment comes in as a
//! closure so tests and the daemon share one code path.

use std::path::{Path, PathBuf};

/// Resolved filesystem layout for one shep home
///
/// All paths are derived from `$SHEP_HOME` (default `<home>/.shep`); nothing
/// here touches the filesystem. The root itself is created by the CLI's own
/// `ensure_home`, for the commands that need it before any daemon exists
/// (`startup` above all), and everything under it by
/// `shep_daemon::boot::init_dirs` on each boot.
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
    /// The control address the client dials and the daemon answers on.
    ///
    /// **Two different kinds of thing behind one field, on purpose.** On
    /// unix it is a filesystem path, `run/shep.sock`, and a real AF_UNIX
    /// socket file lives there. On Windows it is [`Self::pipe_name`] — a
    /// named pipe's `\\.\pipe\...` name, which is path-*shaped* but names an
    /// object in the kernel's pipe namespace rather than a file on any
    /// volume.
    ///
    /// One field rather than two because every consumer in the workspace
    /// treats this as an opaque address it hands to `Client::connect`, and a
    /// second field would make all of them choose. The one place the
    /// difference is load-bearing is a caller that treats this as a *file* —
    /// `shep-cli`'s `wait_for_socket_to_disappear` is the only one, and it
    /// carries its own Windows arm because a pipe has no directory entry to
    /// watch: it stops existing when its last handle closes, so "has the
    /// daemon gone" is a connect attempt there, not a `Path::exists`.
    ///
    /// A corollary worth stating because it silently breaks otherwise:
    /// `socket.parent()` is `$SHEP_HOME/run` on unix and the meaningless
    /// `\\.\pipe` on Windows. Nothing may derive a directory from this field.
    pub socket: PathBuf,
    /// Bark history ring: `barks.jsonl`
    pub barks: PathBuf,
    /// Key/value store: `kv.json`
    pub kv: PathBuf,
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
    ///
    /// [`Self::socket`] resolves per-platform — a socket file under `run/` on
    /// unix, a `\\.\pipe\...` name on Windows — for the reason that field's
    /// own doc gives. Everything else is identical on both.
    #[must_use]
    pub fn resolve(env: &dyn Fn(&str) -> Option<String>, home_dir: &Path) -> Self {
        let home = env("SHEP_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir.join(".shep"));
        let run = home.join("run");
        // `mut` is read only by the `cfg(windows)` block below; on unix the
        // value is returned exactly as built.
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut paths = Self {
            daemon_config: home.join("shep.toml"),
            snapshot: home.join("flock.json"),
            logs: home.join("logs"),
            pids: home.join("pids"),
            socket: run.join("shep.sock"),
            barks: home.join("barks.jsonl"),
            kv: home.join("kv.json"),
            run,
            home,
        };
        // Computed from the already-built value rather than inline above,
        // because `pipe_name` reads `self.home` and the struct is what owns
        // that derivation — duplicating the sanitizer here is exactly how
        // the two would drift.
        #[cfg(windows)]
        {
            paths.socket = PathBuf::from(paths.pipe_name());
        }
        paths
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
        assert_eq!(p.barks, Path::new("/home/rin/.shep/barks.jsonl"));
        assert_eq!(p.kv, Path::new("/home/rin/.shep/kv.json"));
    }

    /// The one field that is not the same kind of thing on both platforms —
    /// see [`ShepPaths::socket`]'s own doc. Asserted per-platform rather
    /// than skipped on Windows, because "the socket resolves to the pipe
    /// name" IS the Windows transport's identity and a silent fallback to
    /// `run/shep.sock` there would produce a daemon that binds a pipe and a
    /// client that dials a file that does not exist.
    #[test]
    fn the_control_address_is_a_socket_file_on_unix_and_a_pipe_name_on_windows() {
        let p = ShepPaths::resolve(&no_env, Path::new("/home/rin"));
        #[cfg(unix)]
        assert_eq!(p.socket, Path::new("/home/rin/.shep/run/shep.sock"));
        #[cfg(windows)]
        assert_eq!(p.socket, Path::new(r"\\.\pipe\shep-home-rin--shep"));
        #[cfg(windows)]
        assert_eq!(
            p.socket,
            Path::new(&p.pipe_name()),
            "the resolved address and `pipe_name` must not drift"
        );
    }

    #[test]
    fn shep_home_env_overrides_root() {
        let env = |key: &str| (key == "SHEP_HOME").then(|| "/srv/shep".to_string());
        let p = ShepPaths::resolve(&env, Path::new("/home/rin"));
        assert_eq!(p.home, Path::new("/srv/shep"));
        #[cfg(unix)]
        assert_eq!(p.socket, Path::new("/srv/shep/run/shep.sock"));
        #[cfg(windows)]
        assert_eq!(p.socket, Path::new(r"\\.\pipe\shep-srv-shep"));
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
