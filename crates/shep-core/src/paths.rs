//! On-disk layout of `$SHEP_HOME`
//!
//! One resolver, no hidden `std::env` reads: the environment comes in as a
//! closure so tests and the daemon share one code path.

use std::path::{Path, PathBuf};

/// Drops the `\\?\` extended-length prefix Windows' `canonicalize` adds
///
/// For paths leaving shep: written to config, shown to an operator, or
/// handed to another program. Paths compared against each other internally,
/// such as `serve`'s docroot containment check, must stay canonical on both
/// sides and must not go through this.
///
/// Only unwraps `\\?\C:\`; a verbatim UNC path (`\\?\UNC\server\share`)
/// passes through unchanged. Not for paths above `MAX_PATH`, where the
/// prefix is load-bearing rather than decorative.
#[cfg(windows)]
#[must_use]
pub fn strip_verbatim_prefix(path: &Path) -> std::borrow::Cow<'_, Path> {
    use std::path::{Component, Prefix};

    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return std::borrow::Cow::Borrowed(path);
    };
    let Prefix::VerbatimDisk(letter) = prefix.kind() else {
        return std::borrow::Cow::Borrowed(path);
    };

    let mut rebuilt = PathBuf::from(format!("{}:\\", char::from(letter)));
    rebuilt.extend(components.filter(|part| !matches!(part, Component::RootDir)));
    std::borrow::Cow::Owned(rebuilt)
}

/// Passes the path through: only Windows' `canonicalize` prefixes its output
///
/// See the Windows sibling for what this exists to undo.
#[cfg(not(windows))]
#[must_use]
pub fn strip_verbatim_prefix(path: &Path) -> std::borrow::Cow<'_, Path> {
    std::borrow::Cow::Borrowed(path)
}

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
    /// A dog's own settings: `dogs.toml`
    ///
    /// Separate from [`Self::daemon_config`] rather than a section inside
    /// it, so lookout can write a dog's config without writing into the
    /// daemon's own hand-authored file.
    pub dogs_config: PathBuf,
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
    /// Unix: a filesystem path, `run/shep.sock`, naming a real AF_UNIX
    /// socket file. Windows: [`Self::pipe_name`], path-shaped but naming an
    /// object in the kernel's pipe namespace, not a file on any volume.
    /// Never derive a directory from this field: `socket.parent()` is
    /// meaningless on Windows. A pipe has no directory entry to watch, so
    /// "has the daemon gone" needs a connect attempt there, not
    /// `Path::exists`.
    pub socket: PathBuf,
    /// Bark history ring: `barks.jsonl`
    pub barks: PathBuf,
    /// Key/value store: `kv.json`
    pub kv: PathBuf,
    /// Operator override store: `overrides.json`
    pub overrides: PathBuf,
}

/// FNV-1a, 64-bit, over `bytes`
///
/// Hand-rolled rather than reached for from `std`: [`std::hash::DefaultHasher`]
/// does not promise a stable value across toolchains, and the daemon and a
/// client built separately have to derive one pipe name and agree on it.
fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

impl ShepPaths {
    /// Windows named-pipe identity for this home:
    /// `\\.\pipe\shep-<sanitized>-<digest>`
    ///
    /// The sanitized stem is not unique alone: `\`, `:`, `.`, `_` and a
    /// literal `-` all collapse to `-`, so `C:\a\b` and `C:\a-b` sanitize to
    /// one string. The digest of the full home path is what keeps two homes
    /// distinct; without it a collision would not error, it would refuse the
    /// second daemon as already running and let its CLI drive the first
    /// home's flock.
    ///
    /// Changing this derivation breaks any already-running daemon: it stays
    /// bound under a name a client built afterward would never dial.
    #[must_use]
    pub fn pipe_name(&self) -> String {
        // Bounds the readable half; a pipe name may be 256 characters.
        const MAX_STEM: usize = 64;

        let home = self.home.to_string_lossy();
        let sanitized: String = home
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let trimmed = sanitized.trim_matches('-');
        // Every character above is ASCII, so this cut cannot split one.
        let stem = trimmed[..trimmed.len().min(MAX_STEM)].trim_end_matches('-');
        let digest = fnv1a64(home.as_bytes());
        format!(r"\\.\pipe\shep-{stem}-{digest:016x}")
    }

    /// Resolves the layout from an environment lookup and the user's home dir
    ///
    /// [`Self::socket`] resolves per-platform: a socket file under `run/` on
    /// unix, [`Self::pipe_name`] on Windows. Everything else is identical.
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
            dogs_config: home.join("dogs.toml"),
            snapshot: home.join("flock.json"),
            logs: home.join("logs"),
            pids: home.join("pids"),
            socket: run.join("shep.sock"),
            barks: home.join("barks.jsonl"),
            kv: home.join("kv.json"),
            overrides: home.join("overrides.json"),
            run,
            home,
        };
        // Computed here, not inlined above: `pipe_name` reads `self.home`,
        // and duplicating the sanitizer here would let the two drift.
        #[cfg(windows)]
        {
            paths.socket = PathBuf::from(paths.pipe_name());
        }
        paths
    }
}

#[cfg(test)]
mod tests {
    /// Unit-level because no end-to-end case can pin this reliably: Node's
    /// handling of a `\\?\` path differs by version.
    #[cfg(windows)]
    #[test]
    fn a_verbatim_prefix_is_stripped() {
        let rewritten = super::strip_verbatim_prefix(std::path::Path::new(r"\\?\C:\tmp\flock.js"));
        assert_eq!(
            rewritten.as_os_str(),
            std::ffi::OsStr::new(r"C:\tmp\flock.js"),
            "node reads the leading `\\\\` as a UNC share and lstats `C:`, so \
             the verbatim prefix must not reach it"
        );

        let plain = std::path::Path::new(r"C:\tmp\flock.js");
        assert_eq!(
            super::strip_verbatim_prefix(plain).as_os_str(),
            plain.as_os_str(),
            "a path with no verbatim prefix must pass through untouched"
        );
    }

    /// Guards the assumption that `canonicalize` really prefixes the path.
    /// If a future Windows or std stops adding it, this stays green and the
    /// strip becomes a no-op rather than a wrong answer.
    #[cfg(windows)]
    #[test]
    fn a_real_canonicalized_path_comes_back_free_of_the_prefix() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("dog.exe");
        std::fs::write(&file, b"not really an exe").expect("write file");

        let canonical = std::fs::canonicalize(&file).expect("canonicalize");
        let rewritten = super::strip_verbatim_prefix(&canonical);
        let shown = rewritten.display().to_string();

        assert!(
            !shown.starts_with(r"\\?\"),
            "the path an operator will read still carries a verbatim prefix: {shown}"
        );
        assert!(
            std::path::Path::new(&shown).is_file(),
            "stripping the prefix must not break the path: {shown}"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn a_unix_path_passes_through_untouched() {
        let plain = std::path::Path::new("/tmp/flock.js");
        assert_eq!(
            super::strip_verbatim_prefix(plain).as_os_str(),
            plain.as_os_str(),
            "the non-Windows arm must be an identity"
        );
    }

    use super::*;
    use std::path::Path;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn default_layout_under_home_dir() {
        let p = ShepPaths::resolve(&no_env, Path::new("/home/ada"));
        assert_eq!(p.home, Path::new("/home/ada/.shep"));
        assert_eq!(p.daemon_config, Path::new("/home/ada/.shep/shep.toml"));
        assert_eq!(p.dogs_config, Path::new("/home/ada/.shep/dogs.toml"));
        assert_eq!(p.snapshot, Path::new("/home/ada/.shep/flock.json"));
        assert_eq!(p.logs, Path::new("/home/ada/.shep/logs"));
        assert_eq!(p.pids, Path::new("/home/ada/.shep/pids"));
        assert_eq!(p.run, Path::new("/home/ada/.shep/run"));
        assert_eq!(p.barks, Path::new("/home/ada/.shep/barks.jsonl"));
        assert_eq!(p.kv, Path::new("/home/ada/.shep/kv.json"));
        assert_eq!(p.overrides, Path::new("/home/ada/.shep/overrides.json"));
    }

    /// Asserted per-platform rather than skipped on Windows: a silent
    /// fallback to `run/shep.sock` there would leave a daemon bound to a
    /// pipe and a client dialing a file that does not exist.
    #[test]
    fn the_control_address_is_a_socket_file_on_unix_and_a_pipe_name_on_windows() {
        let p = ShepPaths::resolve(&no_env, Path::new("/home/ada"));
        #[cfg(unix)]
        assert_eq!(p.socket, Path::new("/home/ada/.shep/run/shep.sock"));
        #[cfg(windows)]
        assert_eq!(
            p.socket,
            Path::new(r"\\.\pipe\shep-home-ada--shep-fd394cfc5c93ad12")
        );
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
        let p = ShepPaths::resolve(&env, Path::new("/home/ada"));
        assert_eq!(p.home, Path::new("/srv/shep"));
        #[cfg(unix)]
        assert_eq!(p.socket, Path::new("/srv/shep/run/shep.sock"));
        #[cfg(windows)]
        assert_eq!(
            p.socket,
            Path::new(r"\\.\pipe\shep-srv-shep-23b467803966a71a")
        );
    }

    #[test]
    fn pipe_name_is_per_home_and_sanitized() {
        // Both homes come from the env, not the default join: its separator
        // is host-specific and would give the digest a different value per
        // platform.
        let env = |key: &str| (key == "SHEP_HOME").then(|| "/home/ada/.shep".to_string());
        let p = ShepPaths::resolve(&env, Path::new("/home/ada"));
        assert_eq!(
            p.pipe_name(),
            r"\\.\pipe\shep-home-ada--shep-626b4d544f86fe95"
        );
        let env = |key: &str| (key == "SHEP_HOME").then(|| "/srv/shep".to_string());
        let q = ShepPaths::resolve(&env, Path::new("/home/ada"));
        assert_eq!(q.pipe_name(), r"\\.\pipe\shep-srv-shep-23b467803966a71a");
    }

    /// The sanitizer is not injective: `\`, `:` and `-` all become `-`. A
    /// collision would not error; it would refuse the second daemon as
    /// already running.
    #[test]
    fn two_homes_that_sanitize_alike_get_distinct_pipe_names() {
        let nested = |key: &str| (key == "SHEP_HOME").then(|| r"C:\a\b".to_string());
        let dashed = |key: &str| (key == "SHEP_HOME").then(|| r"C:\a-b".to_string());
        let n = ShepPaths::resolve(&nested, Path::new("/home/ada"));
        let d = ShepPaths::resolve(&dashed, Path::new("/home/ada"));
        assert!(
            n.pipe_name().starts_with(r"\\.\pipe\shep-C--a-b-")
                && d.pipe_name().starts_with(r"\\.\pipe\shep-C--a-b-"),
            "the readable stem is what collides, and it stays readable: {} vs {}",
            n.pipe_name(),
            d.pipe_name()
        );
        assert_ne!(
            n.pipe_name(),
            d.pipe_name(),
            "only the digest keeps two homes that sanitize alike off one pipe"
        );
    }
}
