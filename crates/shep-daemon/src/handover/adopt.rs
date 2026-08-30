//! Rebuilding a successor's Rust-side objects around descriptors it did not
//! open.
//!
//! Every descriptor named here crossed an `execve`: the predecessor cleared
//! `FD_CLOEXEC` on it (see [`super::fds`]) and wrote its number into the
//! blob. Nothing in this module opens a file, binds a socket or creates a
//! pipe. It takes numbers and wraps them, which is what keeps a sheep's
//! output flowing through the same kernel objects it was flowing through
//! before the exec.
//!
//! # Three rules, each with a reason that is easy to lose
//!
//! **`O_APPEND` survives, because nothing is reopened.** A log handle is
//! wrapped, never opened again by path. `O_APPEND` is a file status flag on
//! the open file description, so it crosses the exec with the descriptor and
//! is still set. That matters more than "the handle is writable":
//! [`super::super::tokio_runner`]'s `open_append` records that a handle
//! without `O_APPEND` writes at its own tracked offset, so a `copytruncate`
//! rotator's truncation leaves a sparse hole the size of everything rotated
//! away. A reopen here would pass a naive write test and corrupt the next
//! rotation.
//!
//! **The pidfile lock is adopted, never re-acquired.** `flock` is a property
//! of the open file description too, so the lock crossed the exec and is
//! still held. Taking it again would mean releasing it first, and that
//! window is exactly long enough for a second daemon to win this home while
//! the only one supervising its flock is mid-boot. All this module does is
//! take ownership of the descriptor, so nothing closes it for the rest of
//! the process's life.
//!
//! **A descriptor the blob names and the process does not have refuses the
//! whole rehydrate.** Not a fresh one in its place, and not a `None`.
//! Supervising a flock with one sheep's output going nowhere is worse than
//! refusing, because the sheep does not lose its output: it blocks on
//! `write()` once the 64KiB pipe buffer fills, and hangs, which reads as an
//! application bug rather than a shep one.
//!
//! # What a failure here leaves behind
//!
//! The pidfile is adopted LAST. Any refusal before that point leaves its
//! descriptor open and unowned, which is what keeps its `flock` held: a
//! successor that cannot rehydrate must not hand this home to a second
//! daemon on its way out. See [`adopt`]'s own doc for what the caller does
//! with the refusal.

use std::fs::File;
use std::io;
use std::os::fd::{OwnedFd, RawFd};

use tokio::net::unix::pipe;

use super::{CarriedFds, CarriedSheep, Handover};
use crate::sys;

/// Everything a successor was handed, rebuilt into objects it can use.
///
/// `Debug` is derived: this carries descriptor numbers, a socket and a
/// sheep's name and pid, all of which an operator can read out of `ps`, and
/// no environment value ever reaches it (see [`Handover`]).
#[derive(Debug)]
pub struct Adopted {
    /// The control listener, on the same socket the predecessor was serving.
    pub listener: tokio::net::UnixListener,
    /// Every sheep the blob described, in the blob's own order.
    pub sheep: Vec<AdoptedSheep>,
    /// The pidfile, held for the process's life so its `flock` is not
    /// released. Never written through here: an `execve` keeps the pid, so
    /// the number already in the file is this process's own.
    pub pidfile: File,
}

/// One sheep's output plumbing, rebuilt.
///
/// `None` on all four means an instance that is registered and not running,
/// which is the only reason a blob names no descriptor for it. A descriptor
/// that is named and missing is a refusal, not a `None`; see this module's
/// own docs.
#[derive(Debug)]
pub struct AdoptedSheep {
    /// What the blob said about this sheep.
    pub carried: CarriedSheep,
    /// The read end of its stdout pipe, as an async reader.
    pub out_pipe: Option<pipe::Receiver>,
    /// The read end of its stderr pipe, as an async reader.
    pub err_pipe: Option<pipe::Receiver>,
    /// The appending handle on its stdout log file.
    pub out_log: Option<tokio::fs::File>,
    /// The appending handle on its stderr log file.
    pub err_log: Option<tokio::fs::File>,
}

/// Rebuild everything `blob` describes, around descriptors this process
/// inherited rather than opened.
///
/// Order is deliberate: the listener, then every sheep, then the pidfile.
/// See this module's own docs for why the pidfile goes last.
///
/// # Errors
///
/// Any descriptor the blob names is not open in this process, is not the
/// kind of object it was named as (a read end that is not a pipe), or could
/// not be registered with the runtime. The error names the sheep and the
/// stream, because that is what an operator needs in order to know which
/// process is now unsupervised.
///
/// There is no partial success and no fallback. By the time this runs the
/// predecessor has already `execve`d itself away, so there is no image left
/// to hand the flock back to; a caller that cannot rehydrate refuses to boot
/// rather than starting a second copy of a flock that is still running.
///
/// # Panics
///
/// Panics if called outside a tokio runtime with IO enabled. Every object
/// built here registers with the runtime's reactor, which has nowhere to
/// happen without one.
#[track_caller]
pub fn adopt(blob: &Handover) -> io::Result<Adopted> {
    let listener = adopt_listener(blob.listener_fd)?;
    let sheep = blob
        .sheep
        .iter()
        .map(adopt_sheep)
        .collect::<io::Result<Vec<_>>>()?;
    // Last, and the whole reason this function has an order worth writing
    // down: an earlier refusal leaves this descriptor open and unowned, so
    // the `flock` it carries stays held.
    let pidfile = adopt_fd(blob.pidfile_fd, "the pidfile lock")?;
    Ok(Adopted {
        listener,
        sheep,
        pidfile,
    })
}

/// Remove the blob at `path`, now that its descriptors are adopted.
///
/// Called only after [`adopt`] has succeeded: a blob left on disk after a
/// refusal is evidence an operator can read, while one left after a success
/// is a picture of a handover that has already happened and would be adopted
/// again by the next boot.
///
/// A failure to remove it is logged rather than returned. The flock is
/// already rehydrated by this point, and refusing to serve it over a leftover
/// file would be a worse trade than the stale blob is a risk.
pub fn discard_blob(path: &std::path::Path) {
    if let Err(error) = std::fs::remove_file(path) {
        tracing::warn!(
            path = %path.display(),
            %error,
            "the handover blob could not be removed after it was adopted"
        );
    }
}

/// Take ownership of `fd`, reporting what it was for when it is not open.
fn adopt_fd(fd: RawFd, what: &str) -> io::Result<File> {
    sys::adopt_handover_fd(fd).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{what} did not survive the handover: {error}"),
        )
    })
}

/// Rebuild the control listener on the descriptor it was already bound to.
///
/// Non-blocking mode is set here rather than assumed. It is a file status
/// flag and does survive the exec, but `tokio::net::UnixListener::from_std`
/// REFUSES a blocking socket rather than fixing one, so a listener that
/// somehow arrived blocking would take down the successor's whole control
/// plane; one `fcntl` is cheaper than depending on an inherited flag.
fn adopt_listener(fd: RawFd) -> io::Result<tokio::net::UnixListener> {
    let file = adopt_fd(fd, "the control listener")?;
    let listener = std::os::unix::net::UnixListener::from(OwnedFd::from(file));
    listener.set_nonblocking(true)?;
    tokio::net::UnixListener::from_std(listener)
}

/// Rebuild one sheep's four handles.
fn adopt_sheep(carried: &CarriedSheep) -> io::Result<AdoptedSheep> {
    let CarriedFds {
        out_pipe,
        err_pipe,
        out_log,
        err_log,
    } = carried.fds;
    let name = &carried.name;
    Ok(AdoptedSheep {
        out_pipe: adopt_pipe(out_pipe, name, "stdout")?,
        err_pipe: adopt_pipe(err_pipe, name, "stderr")?,
        out_log: adopt_log(out_log, name, "stdout")?,
        err_log: adopt_log(err_log, name, "stderr")?,
        carried: carried.clone(),
    })
}

/// Rebuild one pipe read end as an async reader, if the blob named one.
///
/// `pipe::Receiver::from_file` checks that the descriptor really is a pipe
/// open for reading and puts it in non-blocking mode itself, so a blob that
/// crossed two numbers is refused here rather than pumped from a file that
/// never produces a line.
fn adopt_pipe(fd: Option<RawFd>, sheep: &str, stream: &str) -> io::Result<Option<pipe::Receiver>> {
    let Some(fd) = fd else { return Ok(None) };
    let file = adopt_fd(fd, &format!("sheep '{sheep}' {stream} pipe"))?;
    pipe::Receiver::from_file(file).map(Some).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("sheep '{sheep}' {stream} pipe is not a readable pipe: {error}"),
        )
    })
}

/// Rebuild one log handle, if the blob named one.
///
/// Wrapped, never reopened, which is what preserves `O_APPEND` — see this
/// module's own docs for why that is load-bearing rather than tidy.
fn adopt_log(fd: Option<RawFd>, sheep: &str, stream: &str) -> io::Result<Option<tokio::fs::File>> {
    let Some(fd) = fd else { return Ok(None) };
    let file = adopt_fd(fd, &format!("sheep '{sheep}' {stream} log"))?;
    Ok(Some(tokio::fs::File::from_std(file)))
}

#[cfg(test)]
mod tests {
    use std::os::fd::{IntoRawFd as _, RawFd};
    use std::path::Path;

    use tokio::io::{AsyncSeekExt as _, AsyncWriteExt as _};

    use super::adopt;
    use crate::handover::{CarriedFds, CarriedSheep, Handover, VERSION};
    use crate::privilege::SpawnIdentity;
    use shep_core::status::ProcStatus;

    /// One carried sheep named `web`, whose descriptors are `fds`.
    fn carried(fds: CarriedFds) -> CarriedSheep {
        CarriedSheep {
            id: 1,
            name: "web".to_owned(),
            instance: 0,
            pid: Some(100),
            restarts: 0,
            epoch: 7,
            status: ProcStatus::Online,
            last_exit: None,
            credentials: SpawnIdentity::Resolved(None),
            fds,
        }
    }

    /// A blob naming a real listener bound at `socket`, a real pidfile, and
    /// `sheep`.
    ///
    /// Both are opened here rather than left out because `adopt` refuses a
    /// blob whose listener or pidfile is not open, so there is no such
    /// thing as a test blob without them.
    fn blob_with(socket: &Path, sheep: Vec<CarriedSheep>) -> Handover {
        let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
        let pidfile = tempfile::tempfile().unwrap();
        Handover {
            version: VERSION,
            sheep,
            listener_fd: listener.into_raw_fd(),
            pidfile_fd: pidfile.into_raw_fd(),
            next_id: 9,
            next_deadline: 5,
            next_action_stamp: 2,
        }
    }

    #[tokio::test]
    async fn an_adopted_listener_accepts() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("shep.sock");
        let blob = blob_with(&socket, Vec::new());

        let adopted = adopt(&blob).unwrap();

        let listener = adopted.listener;
        let accept = tokio::spawn(async move { listener.accept().await });
        let _client = tokio::net::UnixStream::connect(&socket).await.unwrap();
        accept.await.unwrap().expect("the adopted listener accepts");
    }

    #[tokio::test]
    async fn an_adopted_log_handle_still_appends() {
        // Not merely writable: appending. A handle reopened without
        // `O_APPEND` passes a naive write test and corrupts a rotation, so
        // the assertion is that a write at offset 0 still lands at the end.
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("shep.sock");
        let log = dir.path().join("web-out.log");
        std::fs::write(&log, b"first\n").unwrap();
        let handle = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        let blob = blob_with(
            &socket,
            vec![carried(CarriedFds {
                out_pipe: None,
                err_pipe: None,
                out_log: Some(handle.into_raw_fd()),
                err_log: None,
            })],
        );

        let mut adopted = adopt(&blob).unwrap();

        let mut out = adopted.sheep[0].out_log.take().expect("an adopted log");
        out.seek(std::io::SeekFrom::Start(0)).await.unwrap();
        out.write_all(b"second\n").await.unwrap();
        out.flush().await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "first\nsecond\n",
            "a write at offset 0 overwrote the file, so O_APPEND was lost"
        );
    }

    #[tokio::test]
    async fn a_blob_naming_a_descriptor_that_is_not_open_fails_loudly() {
        // Better to refuse the whole rehydrate than to supervise a flock
        // with one sheep's output silently going nowhere: the child does not
        // lose its output, it blocks on `write()` once the pipe buffer fills.
        //
        // fd 4096 is a number this process will never own, the same floor
        // `sys`'s own refusal tests use.
        const NEVER_OPEN: RawFd = 4096;

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("shep.sock");
        let blob = blob_with(
            &socket,
            vec![carried(CarriedFds {
                out_pipe: Some(NEVER_OPEN),
                err_pipe: None,
                out_log: None,
                err_log: None,
            })],
        );

        let err = adopt(&blob).expect_err("a descriptor that is not open must refuse");

        let text = err.to_string();
        assert!(
            text.contains("web"),
            "the refusal must name the sheep: {text}"
        );
        assert!(
            text.contains("stdout"),
            "the refusal must name the stream: {text}"
        );
    }

    #[tokio::test]
    async fn a_refused_rehydrate_leaves_the_pidfile_lock_held() {
        // The pidfile descriptor is adopted last, so a failure anywhere
        // before it leaves that descriptor open and unowned, which is what
        // keeps its `flock` held. Closing it would release this home to a
        // second daemon at the exact moment there is no daemon supervising
        // it.
        const NEVER_OPEN: RawFd = 4096;

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("shep.sock");
        let blob = blob_with(
            &socket,
            vec![carried(CarriedFds {
                out_pipe: Some(NEVER_OPEN),
                err_pipe: None,
                out_log: None,
                err_log: None,
            })],
        );
        let pidfile_fd = blob.pidfile_fd;

        adopt(&blob).expect_err("a descriptor that is not open must refuse");

        assert!(
            nix::fcntl::fcntl(pidfile_fd, nix::fcntl::FcntlArg::F_GETFD).is_ok(),
            "the pidfile descriptor was closed by a failed rehydrate"
        );
    }

    #[tokio::test]
    async fn an_adopted_pipe_reads_what_was_written_before_the_adoption() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("shep.sock");
        let (reader, mut writer) = std::io::pipe().unwrap();
        std::io::Write::write_all(&mut writer, b"a line\n").unwrap();
        drop(writer);
        let blob = blob_with(
            &socket,
            vec![carried(CarriedFds {
                out_pipe: Some(reader.into_raw_fd()),
                err_pipe: None,
                out_log: None,
                err_log: None,
            })],
        );

        let mut adopted = adopt(&blob).unwrap();

        let out = adopted.sheep[0].out_pipe.take().expect("an adopted pipe");
        let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(out));
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("a line"));
    }

    #[tokio::test]
    async fn a_log_file_offered_as_a_pipe_is_refused() {
        // The read ends are checked to really be pipes, so a blob that
        // crossed two numbers is refused rather than pumped from a file
        // that never produces a line.
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("shep.sock");
        // `into_raw_fd`, because `adopt` takes ownership of whatever the
        // blob names: leaving a `File` owning the same number would double
        // close it.
        let file = tempfile::tempfile().unwrap();
        let blob = blob_with(
            &socket,
            vec![carried(CarriedFds {
                out_pipe: Some(file.into_raw_fd()),
                err_pipe: None,
                out_log: None,
                err_log: None,
            })],
        );

        let err = adopt(&blob).expect_err("a file is not a pipe");

        assert!(err.to_string().contains("web"), "{err}");
    }
}
