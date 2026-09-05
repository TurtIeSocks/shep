//! Spawns `shep daemon`, this binary re-executed with a hidden `daemon`
//! subcommand, detached from the parent's process group and terminal, for
//! `shep_client::spawn::connect_or_spawn`'s autostart path.
//!
//! `Command::process_group(0)` plus redirected stdio does this without a
//! double-fork or `unsafe`. Redirecting stdio only replaces fds 0/1/2;
//! [`seal_inherited_fds`] closes everything else this process holds above
//! them so the daemon inherits none of it.

use std::fs::File;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt as _;
#[cfg(unix)]
use std::os::unix::io::RawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};

#[cfg(unix)]
use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use shep_core::paths::ShepPaths;

/// The shepherd's own stdout, inside `$SHEP_HOME/logs/`.
///
/// One owner for the name: this module creates the file and
/// `commands::logs`' `--daemon` flush empties it, and a copy that drifted
/// would leave `shep flush --daemon` truncating a file nothing writes to
/// while the real one grew.
pub const DAEMON_STDOUT_LOG: &str = "shepd.out.log";

/// The shepherd's own stderr, where its `tracing` records land. See
/// [`DAEMON_STDOUT_LOG`] for why the name lives here.
pub const DAEMON_STDERR_LOG: &str = "shepd.err.log";

/// Builds the `shep daemon` command: creates the log directory and opens
/// both log files, but does not spawn it.
///
/// Creates `paths.logs` here since `init_dirs` only runs after `exec`,
/// inside the child. Sets the directory's mode at creation via
/// `DirBuilderExt`, not `create_dir_all` plus `set_permissions`, to avoid
/// a TOCTOU window at the umask.
///
/// # Errors
/// - The log directory could not be created.
/// - Either log file could not be opened for writing.
/// - [`std::env::current_exe`] failed to resolve this binary's own path.
pub fn launch_command(paths: &ShepPaths) -> io::Result<Command> {
    let mut log_dir = std::fs::DirBuilder::new();
    log_dir.recursive(true);
    // Windows has no scalar mode to set; `shep_daemon::boot`'s
    // `create_dir_at_dir_mode` carries the argument for what guards the
    // control plane there instead.
    #[cfg(unix)]
    log_dir.mode(shep_daemon::boot::DIR_MODE);
    log_dir.create(&paths.logs)?;

    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("daemon")
        // Pins the resolved `$SHEP_HOME` so the child binds the same socket
        // the parent resolved, instead of re-resolving from its own environment.
        .env("SHEP_HOME", &paths.home)
        // No `.env_clear()`: the child needs `PATH`, and `DaemonConfig::load`
        // reads `SHEP_*` overrides from its own environment.
        .stdout(emptied_appending(&paths.logs.join(DAEMON_STDOUT_LOG))?)
        .stderr(emptied_appending(&paths.logs.join(DAEMON_STDERR_LOG))?)
        .stdin(Stdio::null());

    // Detaches from the parent's process group so the daemon survives the
    // parent exiting and the terminal closing.
    #[cfg(unix)]
    cmd.process_group(0);

    // Win32 analogue of the unix detach: `DETACHED_PROCESS` drops the
    // console, `CREATE_NEW_PROCESS_GROUP` isolates Ctrl+C, and
    // `CREATE_BREAKAWAY_FROM_JOB` escapes a job object some hosts put
    // children in. `launch_daemon` retries without the last flag if it fails.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
    }

    Ok(cmd)
}

/// Empties `path` while leaving the returned handle open in append mode.
///
/// The daemon inherits this as one of its own stdio fds and never reopens
/// it, so the mode set here is the mode it keeps for its whole life.
/// `File::create` opens `O_TRUNC` without `O_APPEND`; a handle that tracks
/// its own offset then writes past an external truncation instead of at
/// offset 0, leaving a `NUL`-filled hole. `std` refuses `append(true)` with
/// `truncate(true)`, so the empty is a `set_len(0)` on the already-appending
/// handle instead.
///
/// # Errors
/// The file could not be opened, or could not be emptied once open.
fn emptied_appending(path: &Path) -> io::Result<File> {
    // Windows cannot truncate a handle opened append-only: `set_len` needs
    // `FILE_WRITE_DATA`, which append mode lacks. So the empty
    // happens on a separate, short-lived handle before the append-only one
    // is opened.
    #[cfg(windows)]
    File::create(path)?;

    let file = File::options().create(true).append(true).open(path)?;

    #[cfg(unix)]
    file.set_len(0)?;

    Ok(file)
}

/// Lowest descriptor [`seal_inherited_fds`] marks. 0/1/2 are stdio, which
/// [`launch_command`] replaces for the child and which this process still
/// needs for its own output.
#[cfg(unix)]
const FIRST_NON_STDIO_FD: RawFd = 3;

/// The directory whose entries name this process's own open descriptors.
/// `/dev/fd` on macOS (the `fdesc` filesystem), a symlink to
/// `/proc/self/fd` on Linux; both list exactly the numbers this process
/// holds.
#[cfg_attr(windows, allow(dead_code))]
const FD_DIR: &str = "/dev/fd";

/// Marks every descriptor this process holds above stdio close-on-exec, so
/// the daemon inherits none of it.
///
/// A leaked descriptor, typically the launcher's own stdout/stderr pipe,
/// keeps a reader blocked on EOF for as long as the daemon lives: EOF
/// arrives only when the last copy of the write end closes. Leaked
/// descriptors are not enumerable in advance, so this sweeps everything
/// above stdio rather than closing one known fd.
///
/// Marks rather than closes: this process still needs those descriptors for
/// its own output, and every failure here (an unreadable `/dev/fd`, a
/// descriptor closed mid-sweep) is ignored rather than refusing to start.
#[cfg(unix)]
fn seal_inherited_fds() {
    let Ok(entries) = std::fs::read_dir(FD_DIR) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(fd) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<RawFd>().ok())
        else {
            continue;
        };
        if fd < FIRST_NON_STDIO_FD {
            continue;
        }
        let _ = fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC));
    }
}

/// Spawns `shep daemon`, detached from this process's group and terminal.
///
/// Returns the child unwaited so the caller can `try_wait()` it while
/// probing for readiness.
///
/// [`seal_inherited_fds`] runs before [`launch_command`]: the two log files
/// it opens are already close-on-exec and reach the child through `dup2`
/// regardless, so ordering the sweep first keeps them out of its path.
///
/// # Errors
/// Whatever [`launch_command`] can fail with, plus the spawn itself.
#[cfg(unix)]
pub fn launch_daemon(paths: &ShepPaths) -> io::Result<Child> {
    seal_inherited_fds();
    launch_command(paths)?.spawn()
}

/// Windows' `DETACHED_PROCESS`: the child gets no console of its own and
/// does not inherit the parent's.
#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;
/// Windows' `CREATE_NEW_PROCESS_GROUP`.
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
/// Windows' `CREATE_BREAKAWAY_FROM_JOB`.
#[cfg(windows)]
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

/// Spawns the detached shepherd.
///
/// `CreateProcess` with `bInheritHandles = TRUE` (set whenever stdio is
/// redirected) hands over every inheritable handle the parent holds, not
/// only the three `Command` marks; `seal_std_handles` closes the rest.
///
/// `CREATE_BREAKAWAY_FROM_JOB` fails the spawn with `ERROR_ACCESS_DENIED`
/// when the containing job forbids breakaway, so a failed spawn retries
/// once without it, bound to that job's lifetime instead.
///
/// # Errors
/// Whatever [`launch_command`] can fail with, plus the spawn itself.
#[cfg(windows)]
pub fn launch_daemon(paths: &ShepPaths) -> io::Result<Child> {
    use std::os::windows::process::CommandExt as _;

    shep_daemon::sys_windows::seal_std_handles();
    match launch_command(paths)?.spawn() {
        Ok(child) => Ok(child),
        Err(err) if err.raw_os_error() == Some(5) => {
            let mut cmd = launch_command(paths)?;
            cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
            cmd.spawn()
        }
        Err(err) => Err(err),
    }
}

// This module's unix tests assert unix specifics only. The Windows arm
// (detached creation flags, breakaway retry) is exercised end to end by
// `tests/cli_e2e.rs`, which starts a real detached shepherd.
#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    use super::*;

    /// Builds a [`ShepPaths`] rooted at `dir`; each test needs its own,
    /// unshared.
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

    /// A cold `$SHEP_HOME` has no log directory yet; without this the
    /// redirects below fail with `ENOENT`.
    #[test]
    fn the_launcher_creates_the_log_directory_before_spawning() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        assert!(!paths.logs.exists(), "precondition: a cold $SHEP_HOME");

        let _cmd = launch_command(&paths).unwrap();

        assert!(paths.logs.is_dir(), "the redirect targets must be openable");
        assert_eq!(mode_of(&paths.logs), shep_daemon::boot::DIR_MODE);
    }

    #[test]
    fn the_daemons_own_log_survives_a_truncation_without_a_hole() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shepd.err.log");

        let mut inherited = emptied_appending(&path).unwrap();
        inherited.write_all(b"aaaaaaaaaa").unwrap();
        inherited.flush().unwrap();

        // `shep flush --daemon`, from outside, exactly as the CLI does it.
        File::options()
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();

        inherited.write_all(b"bbb").unwrap();
        inherited.flush().unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            3,
            "a descriptor keeping its own offset would leave 13 bytes here, the first ten of \
             them NUL"
        );
    }

    /// Guards against `emptied_appending` regressing to a plain truncate.
    #[test]
    fn relaunching_still_starts_the_daemons_own_logs_empty() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(&dir);
        std::fs::create_dir_all(&paths.logs).unwrap();
        let path = paths.logs.join(DAEMON_STDOUT_LOG);
        std::fs::write(&path, b"a previous daemon's output").unwrap();

        let _cmd = launch_command(&paths).unwrap();

        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
    }

    /// Whether `fd` is marked close-on-exec right now.
    #[cfg(unix)]
    fn is_close_on_exec(fd: RawFd) -> bool {
        let flags = fcntl(fd, FcntlArg::F_GETFD).unwrap();
        FdFlag::from_bits_truncate(flags).contains(FdFlag::FD_CLOEXEC)
    }

    /// `FD_CLOEXEC` is cleared manually first: std opens every descriptor
    /// close-on-exec already, so the fixture has to simulate an inherited one.
    #[test]
    fn the_sweep_marks_an_inherited_descriptor_close_on_exec() {
        let (leaked, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let fd = std::os::unix::io::AsRawFd::as_raw_fd(&leaked);
        fcntl(fd, FcntlArg::F_SETFD(FdFlag::empty())).unwrap();
        assert!(!is_close_on_exec(fd), "the fixture must start inheritable");

        seal_inherited_fds();

        assert!(
            is_close_on_exec(fd),
            "fd {fd} would cross the exec and be held for the daemon's whole life"
        );
    }

    /// Checked on stdin, not stdout/stderr: the test harness may redirect
    /// those two itself.
    #[test]
    fn the_sweep_leaves_stdio_alone() {
        assert!(
            !is_close_on_exec(0),
            "fixture: this process's own stdin is inherited and inheritable"
        );

        seal_inherited_fds();

        assert!(!is_close_on_exec(0), "the sweep must start above stdio");
    }
}
