//! Spawns `shep daemon` — this binary re-executed with its own hidden
//! `daemon` subcommand — detached from the parent's process group and
//! terminal, for `shep_client::spawn::connect_or_spawn`'s autostart path.
//!
//! No double-fork: `Command::process_group(0)` (stable since Rust 1.64 via
//! `std::os::unix::process::CommandExt`) plus redirected stdio is enough
//! for the daemon to survive the parent process exiting and its
//! controlling terminal closing, without any `unsafe` — and this crate is
//! `#![forbid(unsafe_code)]`.
//!
//! Redirecting stdio is not by itself enough to make the daemon *clean* of
//! its launcher, though: only fds 0/1/2 are replaced, and anything above
//! them that this process happens to hold without `FD_CLOEXEC` survives the
//! `exec` and is then held for the daemon's whole life. [`seal_inherited_fds`]
//! is what closes that, and its own doc carries the bug it was written for.

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

/// The shepherd's own stderr — where its `tracing` records land. See
/// [`DAEMON_STDOUT_LOG`] for why the name lives here.
pub const DAEMON_STDERR_LOG: &str = "shepd.err.log";

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
/// Reached in production through [`launch_daemon`], which `main` hands to
/// `shep_client::spawn::connect_or_spawn` as its launcher — the binary's
/// only autostart.
pub fn launch_command(paths: &ShepPaths) -> io::Result<Command> {
    let mut log_dir = std::fs::DirBuilder::new();
    log_dir.recursive(true);
    // Mode at creation, never a later `chmod` — see this function's doc.
    // Windows has no scalar mode to set; `shep_daemon::boot`'s
    // `create_dir_at_dir_mode` carries the argument for what guards the
    // control plane there instead.
    #[cfg(unix)]
    log_dir.mode(shep_daemon::boot::DIR_MODE);
    log_dir.create(&paths.logs)?;

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
        .stdout(emptied_appending(&paths.logs.join(DAEMON_STDOUT_LOG))?)
        .stderr(emptied_appending(&paths.logs.join(DAEMON_STDERR_LOG))?)
        .stdin(Stdio::null());

    // Detaches from the parent's process group so the daemon survives the
    // parent exiting and its controlling terminal closing — no double-fork,
    // no unsafe (`process_group` is `std::os::unix::process::CommandExt`,
    // stable since Rust 1.64).
    #[cfg(unix)]
    cmd.process_group(0);

    // The same detachment, spelled in Win32 creation flags. All three are
    // load-bearing and none is the default:
    //
    // - `DETACHED_PROCESS` gives the daemon no console at all, so closing
    //   the terminal that ran `shep start` cannot deliver `CTRL_CLOSE_EVENT`
    //   to it. This is the direct analogue of leaving the parent's process
    //   group, and without it the shepherd dies with the window that
    //   launched it — the exact failure `process_group(0)` exists to prevent.
    // - `CREATE_NEW_PROCESS_GROUP` additionally roots a group at the daemon,
    //   so a Ctrl+C in the launching console is not broadcast to it.
    // - `CREATE_BREAKAWAY_FROM_JOB` matters when shep itself was started
    //   inside a job object — which is ordinary on Windows: some terminal
    //   hosts, CI runners and IDE test harnesses put their children in one
    //   with `KILL_ON_JOB_CLOSE` set. Without breakaway the shepherd would
    //   inherit that job and be terminated when the harness closed it,
    //   taking the whole flock with it.
    //
    // `CREATE_BREAKAWAY_FROM_JOB` fails the spawn outright if the containing
    // job forbids breakaway, so it is not set unconditionally — see
    // `launch_daemon`, which retries without it.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
    }

    Ok(cmd)
}

/// `File::create`'s effect — the file exists and is empty — on a descriptor
/// opened `O_APPEND`.
///
/// # Why not `File::create`
///
/// The daemon inherits these two as fds 1 and 2 and never opens them itself,
/// so whatever mode they are opened in here is the mode they keep for the
/// daemon's whole life. `File::create` is `O_WRONLY|O_CREAT|O_TRUNC` with no
/// `O_APPEND`, which leaves the descriptor tracking its own offset — and a
/// descriptor tracking its own offset writes PAST an external truncation
/// rather than at offset 0 of the emptied file. Measured: ten bytes written,
/// the file truncated from outside, three more bytes written, and the file
/// is thirteen bytes of which the first ten are `NUL`. Under `O_APPEND` the
/// same sequence leaves three bytes. This is the sparse hole `open_append`'s
/// own doc argues about for a sheep's logs, in the one place shep opens a log
/// file that is not a sheep's — and `shep flush --daemon` is the truncation
/// that would otherwise walk into it.
///
/// The launch-time emptying is preserved rather than traded away: `std`
/// refuses `append(true)` together with `truncate(true)`
/// (`OpenOptions::get_creation_mode` returns `InvalidInput`), so the truncate
/// is a `set_len(0)` on the already-appending handle instead. Reusing one
/// `$SHEP_HOME`'s logs across relaunches is still the whole of their rotation
/// story.
///
/// # Errors
///
/// The file could not be opened, or could not be emptied once open.
fn emptied_appending(path: &Path) -> io::Result<File> {
    // Windows cannot empty an append handle, and the reason is the same
    // property that makes it an append handle.
    //
    // `OpenOptions::append(true)` maps to `FILE_GENERIC_WRITE &
    // !FILE_WRITE_DATA` — std removes `FILE_WRITE_DATA` deliberately,
    // because on Windows "append mode" IS the state of holding
    // `FILE_APPEND_DATA` while lacking `FILE_WRITE_DATA`. `set_len` needs
    // exactly the right std removed, so the unix arm's
    // `open-then-set_len(0)` fails here with `ERROR_ACCESS_DENIED` — which
    // is not a permissions problem an operator can fix, and which surfaced
    // as `shep start` refusing to autostart a shepherd at all.
    //
    // Granting `FILE_WRITE_DATA` back through `access_mode` would fix the
    // truncate and silently destroy the append semantics this function
    // exists to establish, taking the sparse-hole guarantee its doc argues
    // for with it. So the emptying is done on a SEPARATE, short-lived
    // handle that has ordinary write access, and the handle actually handed
    // to the daemon is opened append-only afterwards.
    //
    // Two opens rather than one, and a window between them in which another
    // process could write to a file it has no reason to touch. That is
    // acceptable where reordering is not: an append handle is what the
    // daemon holds for its whole life, and it must be a real one.
    #[cfg(windows)]
    File::create(path)?;

    let file = File::options().create(true).append(true).open(path)?;

    #[cfg(unix)]
    file.set_len(0)?;

    Ok(file)
}

/// Lowest descriptor [`seal_inherited_fds`] touches. 0/1/2 are stdio, which
/// [`launch_command`] replaces wholesale for the child and which this
/// process still needs for its own output afterwards — marking those
/// close-on-exec would change what every OTHER `exec` from this process
/// sees, to fix nothing the redirects have not already fixed.
#[cfg(unix)]
const FIRST_NON_STDIO_FD: RawFd = 3;

/// The directory whose entries name this process's own open descriptors.
/// `/dev/fd` on macOS (the `fdesc` filesystem), a symlink to
/// `/proc/self/fd` on Linux; both list exactly the numbers this process
/// holds.
#[cfg_attr(windows, allow(dead_code))]
const FD_DIR: &str = "/dev/fd";

/// Marks every descriptor this process holds above stdio close-on-exec, so
/// the daemon inherits none of them.
///
/// # The bug this exists for
///
/// A daemon lives for as long as `$SHEP_HOME` has a flock, so ANY
/// descriptor it inherits by accident is held open for that whole time —
/// and the launcher's own stdio is exactly the kind of descriptor that
/// leaks in. `shep start` is normally run with its stdout and stderr on a
/// pipe (a CI runner, `$(shep start …)`, a test harness, a shell reading
/// the output). Whoever holds the read end waits for EOF, and EOF arrives
/// only when the LAST copy of the write end closes — so a copy sitting in
/// the daemon means the reader never returns, long after `shep start`
/// itself has exited and printed everything it had to say. The reader is
/// then blocked, at 0% CPU, on a process that finished minutes ago.
///
/// Reproduced against `cli_e2e`'s `concurrent_cold_starts_produce_exactly_one_daemon`,
/// which is where it was found: under load the case would stall
/// indefinitely — not fail — with both `shep start` processes exited, the
/// surviving daemon holding the write end of one racer's stdout pipe, and
/// the harness parked in `read_to_end` waiting for an EOF that could not
/// come. `assert_cmd`'s `.timeout()` does not bound it: that bounds the
/// *process* wait, and the reader-thread join happens after it.
///
/// # Why the sweep, rather than closing one known descriptor
///
/// Because the leak is not this process's to enumerate. A descriptor
/// arrives without `FD_CLOEXEC` when something `dup2`'d it into place —
/// which is what every parent does to hand us our own stdio, and what a
/// `fork` in a multi-threaded parent can leave behind at other numbers
/// besides. The launcher cannot know which of the numbers it holds are
/// its own and which are a caller's leftovers, so it stops asking: above
/// stdio, nothing at all crosses this `exec`. The daemon receives its
/// stdio through [`launch_command`]'s redirects and opens everything else
/// (socket, pidfile, log files) for itself, so there is nothing left for
/// it to legitimately want.
///
/// Marking rather than closing, and marking *here* rather than in the
/// daemon: this process still needs those descriptors — it is a live
/// client that will go on to connect and print — so closing them is not on
/// offer. `FD_CLOEXEC` costs it nothing and takes effect at exactly the
/// boundary that matters. The daemon cannot do this job for itself either:
/// by the time any of its own code runs, a tokio runtime has already
/// opened descriptors of its own, and nothing in the process can then tell
/// an inherited number from one it just allocated (the same
/// recycled-number hazard `shep_daemon::sys`'s rationale essay works
/// through for `adopt_fd`).
///
/// # Best-effort, deliberately
///
/// Every failure is ignored and the launch proceeds. An unreadable
/// `/dev/fd`, an entry that is not a number, a descriptor closed between
/// the listing and the `fcntl` — none of them is a reason to refuse to
/// start a daemon, because the worst case of doing nothing here is the
/// pre-existing behaviour this function improves on, not a corrupt one.
/// The sweep's own directory handle is in the listing and gets marked
/// along with everything else, which is harmless: it is closed before this
/// function returns.
///
/// Process-wide, so it belongs at a spawn site and not in a builder: a
/// concurrent thread that wanted a descriptor of its own inherited by a
/// child would be defeated by it. Nothing in this binary spawns anything
/// but the daemon.
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
/// Returns the child so the caller (`shep_client::spawn::connect_or_spawn`)
/// can `try_wait()` it while probing for readiness, rather than blocking on
/// it directly.
///
/// [`seal_inherited_fds`] runs first, so the daemon crosses the `exec` with
/// nothing but the stdio [`launch_command`] gives it — see that function's
/// own doc for the hang that motivates it. Ordered before [`launch_command`]
/// rather than after so the two log files it opens are never in the sweep's
/// path at all: they are already close-on-exec (std opens every file that
/// way) and reach the child as stdio through `dup2`, which clears the flag
/// on the descriptor it creates.
///
/// # Errors
/// Whatever [`launch_command`] can fail with, plus the spawn itself.
///
/// This is the launcher `main` passes to
/// `shep_client::spawn::connect_or_spawn`, so a cold `$SHEP_HOME` gets a
/// daemon on the first command that needs one.
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
/// **There IS a `seal_inherited_fds` counterpart here**, and an earlier
/// version of this doc claimed there was not. The claim was that Windows
/// inherits only handles explicitly marked inheritable, so `Command`
/// marking its three stdio handles was already the outcome the unix sweep
/// works to reach. That is true of what `std` prepares and false of what
/// actually gets inherited: `CreateProcess` with `bInheritHandles = TRUE`
/// — which `std` passes whenever stdio is redirected — hands over EVERY
/// inheritable handle the parent holds.
///
/// So a shepherd launched from a shell that gave `shep` a pipe for stdout
/// inherited that pipe and held it open for its whole life, and the shell
/// blocked forever waiting for it to close. `shep start | anything` hung;
/// bare `shep start` did not. `shep_daemon::sys_windows::seal_std_handles`
/// closes it, and is called below for exactly the reason
/// [`seal_inherited_fds`] is called on unix.
///
/// # The breakaway retry
///
/// `CREATE_BREAKAWAY_FROM_JOB` is not merely ignored when the containing job
/// forbids breakaway — `CreateProcess` FAILS, with `ERROR_ACCESS_DENIED`. So
/// asking for it unconditionally would make `shep start` fail outright
/// inside any harness that runs its children in a locked-down job, which is
/// a common shape rather than an exotic one.
///
/// Asking and then retrying without it gets both halves right: a shepherd
/// launched from an ordinary console breaks away and survives its parent,
/// and one launched inside a restrictive job still starts — bound to that
/// job's lifetime, which is the best available answer and strictly better
/// than refusing to start at all.
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

// The whole module asserts unix specifics — a `0700` mode read back off the
// log directory, `process_group` on the built `Command`, and the
// close-on-exec sweep, none of which exists on Windows. What the Windows arm
// claims instead (detached creation flags, the breakaway retry) is exercised
// end to end by `tests/cli_e2e.rs`, which starts a real detached shepherd.
#[cfg(all(test, unix))]
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

    /// Fails if [`emptied_appending`] goes back to `File::create` — the exact
    /// shape this was before `shep flush --daemon` existed.
    ///
    /// This is the measurement, not a proxy for it. Ten bytes, an external
    /// truncation, three more bytes: an `O_APPEND` descriptor seeks to end
    /// before every write and leaves three bytes, while one tracking its own
    /// offset writes at 10 and leaves thirteen, of which the first ten are
    /// `NUL`. Only the LENGTH separates them — both files end with the same
    /// three bytes, and `read_to_string` on either contains what was written.
    ///
    /// The daemon never opens these files itself; it inherits them as fds 1
    /// and 2 and keeps whatever mode they were opened in for its whole life.
    /// So this one call decides whether `shep flush --daemon` empties the
    /// shepherd's log or merely punches a hole in front of it.
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

    /// Fails if the launch-time emptying is dropped along the way to
    /// `O_APPEND` — `std` refuses `append(true)` with `truncate(true)`, so the
    /// obvious rewrite of `File::create` silently turns "one launch, one fresh
    /// log" into an append that grows across every relaunch of the same
    /// `$SHEP_HOME`. That is still the whole of these two files' rotation
    /// story, so losing it is losing the only thing that bounds them.
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

    /// The sweep reaches a descriptor in the state an inherited one is
    /// actually in — open, and NOT close-on-exec — which is the state a
    /// `dup2` leaves behind and the only state that can survive an `exec`.
    ///
    /// `UnixStream::pair` alone would not test anything: std opens every
    /// descriptor close-on-exec already, so the flag is cleared here first
    /// to build the fixture the bug needs. What a broken implementation
    /// this catches: a sweep that skipped the numbers it could not
    /// attribute, or that read the wrong directory and quietly marked
    /// nothing — the daemon would then go on holding this descriptor for
    /// life, which is the hang `seal_inherited_fds`' own doc describes.
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

    /// Stdio is the launcher's own output and the child's, replaced by
    /// `launch_command`'s redirects — the sweep must leave all three alone.
    ///
    /// Asserted on stdin rather than stdout/stderr because the test harness
    /// is entitled to do what it likes with the latter two. What a broken
    /// implementation this catches: a sweep that started from fd 0, which
    /// would change what every other `exec` from this process inherits, to
    /// fix nothing — the daemon replaces all three regardless.
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
