//! Spawns `shep daemon` — this binary re-executed with its own hidden
//! `daemon` subcommand — detached from the parent's process group and
//! terminal, for `shep_client::spawn::connect_or_spawn`'s autostart path.
//!
//! No double-fork: `Command::process_group(0)` (stable since Rust 1.64 via
//! `std::os::unix::process::CommandExt`) plus redirected stdio is enough
//! for the daemon to survive the parent process exiting and its
//! controlling terminal closing, without any `unsafe` — and this crate is
//! `#![forbid(unsafe_code)]`.

use std::fs::File;
use std::io;
use std::os::unix::fs::DirBuilderExt as _;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, Stdio};

use shep_core::paths::ShepPaths;

/// Builds the fully configured `shep daemon` command — log directory
/// created, both log files opened — but does not spawn it.
///
/// Creating `paths.logs` here, before opening the two files below inside
/// it, duplicates one directory of `shep_daemon::boot::init_dirs` on
/// purpose: that function is authoritative and idempotent and still runs,
/// but only *after* `exec`, inside the child. On a cold `$SHEP_HOME`
/// nothing has created `paths.logs` yet at the point this function needs to
/// open files inside it, so without this the redirect below fails with
/// `ENOENT` and the daemon never starts. Do not remove it as
/// "redundant" — that is the exact failure this exists to prevent.
///
/// The `.mode(shep_daemon::boot::DIR_MODE)` below sets the directory's mode
/// at creation, via `DirBuilderExt`, rather than `create_dir_all` followed
/// by a separate `set_permissions` — matching
/// `shep_daemon::boot::create_dir_at_dir_mode`'s own TOCTOU discipline. A
/// create-then-chmod sequence leaves a window in which the directory exists
/// at whatever the ambient umask allows before the chmod narrows it; on a
/// shared machine that window is enough for another user to open a handle
/// that survives the later chmod. Requesting the mode at `mkdir` time
/// leaves no such window. Do not "simplify" this to `create_dir_all`.
///
/// # Errors
/// - The log directory could not be created.
/// - Either log file could not be opened for writing.
/// - [`std::env::current_exe`] failed to resolve this binary's own path.
///
/// Not called outside this module's own tests yet: wiring this launcher
/// into `shep_client::spawn::connect_or_spawn`'s autostart path is the
/// `start` command's own job, and `commands::lifecycle` (which will own
/// `start`) has not landed. `#[allow(dead_code)]` says so explicitly
/// rather than inventing a call site nothing needs yet.
#[allow(dead_code)]
pub fn launch_command(paths: &ShepPaths) -> io::Result<Command> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(shep_daemon::boot::DIR_MODE)
        .create(&paths.logs)?;

    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("daemon")
        // The parent already resolved `$SHEP_HOME` from `--home`/the
        // environment; pinning it explicitly here means the child resolves
        // the same paths deterministically instead of depending on
        // whatever the parent's own ambient environment happens to hold.
        // Letting it inherit would mean `shep --home <path> start` has the
        // parent probing one socket while the child binds another.
        .env("SHEP_HOME", &paths.home)
        // No `.env_clear()`: the child still needs `PATH` to exec anything
        // at all, and clearing would also drop the `SHEP_*` overrides
        // `DaemonConfig::load` reads from the child's own environment.
        //
        // Detaches from the parent's process group so the daemon survives
        // the parent exiting and its controlling terminal closing — no
        // double-fork, no unsafe (`process_group` is
        // `std::os::unix::process::CommandExt`, stable since Rust 1.64).
        .process_group(0)
        .stdout(File::create(paths.logs.join("shepd.out.log"))?)
        .stderr(File::create(paths.logs.join("shepd.err.log"))?)
        .stdin(Stdio::null());
    Ok(cmd)
}

/// Spawns `shep daemon`, detached from this process's group and terminal.
///
/// Returns the child so the caller (`shep_client::spawn::connect_or_spawn`)
/// can `try_wait()` it while probing for readiness, rather than blocking on
/// it directly.
///
/// # Errors
/// Whatever [`launch_command`] can fail with, plus the spawn itself.
///
/// Not called outside this module's own tests yet, for the same reason as
/// [`launch_command`]. `#[allow(dead_code)]` says so explicitly rather than
/// inventing a call site nothing needs yet.
#[allow(dead_code)]
pub fn launch_daemon(paths: &ShepPaths) -> io::Result<Child> {
    launch_command(paths)?.spawn()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    use super::*;

    /// Builds a [`ShepPaths`] rooted at `dir`, the fixture every test in
    /// this module needs and none of them may share (IR-33/34: unique
    /// fixtures per test).
    fn test_paths(dir: &tempfile::TempDir) -> ShepPaths {
        ShepPaths::resolve(
            &|k| (k == "SHEP_HOME").then(|| dir.path().to_string_lossy().into_owned()),
            Path::new("/nonexistent"),
        )
    }

    /// The directory's mode, narrowed to the permission bits `DIR_MODE`
    /// itself is expressed in.
    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_launcher_never_sets_the_readiness_fd_variable() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let cmd = launch_command(&paths).unwrap(); // configured, never spawned
        assert!(
            !cmd.get_envs().any(|(k, _)| k == "SHEP_READY_FD"),
            "the whole phase design rests on readiness being a handshake, not an fd"
        );
    }

    #[test]
    fn the_launcher_pins_shep_home_to_the_resolved_path() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let cmd = launch_command(&paths).unwrap();
        let home = cmd
            .get_envs()
            .find(|(k, _)| *k == "SHEP_HOME")
            .and_then(|(_, v)| v)
            .expect("the child must not re-resolve $SHEP_HOME from ambient environment");
        assert_eq!(Path::new(home), paths.home);
    }

    /// A launcher that forgot `.arg("daemon")` would re-exec `shep` with no
    /// subcommand, print help into `shepd.out.log`, exit 2, and the parent
    /// would report `DaemonExited { status: 2 }` from thirty seconds of
    /// probing.
    #[test]
    fn the_launcher_runs_this_binarys_hidden_daemon_subcommand() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        let cmd = launch_command(&paths).unwrap();
        assert_eq!(
            cmd.get_program(),
            std::env::current_exe().unwrap().as_os_str()
        );
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, ["daemon"]);
    }

    /// The ENOENT that would otherwise sink the phase's headline feature on
    /// first use: on a cold `$SHEP_HOME` the log directory does not exist,
    /// the redirect opens two files inside it, and the daemon's own
    /// `init_dirs` only runs after exec. Because `launch_command` returns
    /// without spawning, "before spawning" is what this test literally
    /// observes.
    #[test]
    fn the_launcher_creates_the_log_directory_before_spawning() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        assert!(!paths.logs.exists(), "precondition: a cold $SHEP_HOME");

        let _cmd = launch_command(&paths).unwrap();

        assert!(paths.logs.is_dir(), "the redirect targets must be openable");
        assert_eq!(mode_of(&paths.logs), shep_daemon::boot::DIR_MODE);
    }
}
