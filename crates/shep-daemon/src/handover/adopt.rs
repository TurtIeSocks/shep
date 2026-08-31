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

/// One sheep's output plumbing, rebuilt, and its input plumbing with it.
///
/// `None` on all of the first four means an instance that is registered and
/// not running, which is the only reason a blob names no descriptor for
/// them. A descriptor that is named and missing is a refusal, not a `None`;
/// see this module's own docs.
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
    /// The write end of its stdin pipe, for a sheep whose app asked for one.
    ///
    /// The only handle here the daemon writes to rather than reads from,
    /// and `None` for the commoner sheep that has `/dev/null` on fd 0.
    pub stdin_pipe: Option<pipe::Sender>,
    /// The daemon's end of its shepherd-channel socketpair, whose other end
    /// is the child's fd 3.
    ///
    /// The only handle here that goes both ways: the successor splits it
    /// into the same reader and writer a spawn wires, over one open file
    /// description. `None` for a sheep whose app asked for no channel, one
    /// that is not running, and one whose child has already closed fd 3.
    pub channel: Option<tokio::net::UnixStream>,
}

/// Rebuild everything `blob` describes, around descriptors this process
/// inherited rather than opened.
///
/// Order is deliberate: the listener, then every sheep, then the pidfile.
/// See this module's own docs for why the pidfile goes last.
///
/// # Errors
///
/// The blob names one descriptor number twice, or any descriptor it names is
/// not open in this process, is not the kind of object it was named as (a
/// read end that is not a pipe, a stdin end that is not writable), or could
/// not be registered with the runtime. The error names the sheep and the stream, because that is what
/// an operator needs in order to know which process is now unsupervised.
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
    refuse_repeated_fds(blob)?;
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

/// Refuses a blob naming one descriptor number more than once.
///
/// Before the first adoption, never during. Each adoption builds an owner of
/// its number, and `sys::adopt_handover_fd`'s safety argument rests on that
/// owner being the only one: a second owner of the same number closes it a
/// second time on drop, and whatever this process opened in between is what
/// the second close reaches.
///
/// A blob the daemon wrote cannot contain a repeat, since every number in it
/// is a distinct open descriptor at the moment of the snapshot. This is for
/// the residual `adopt_handover_fd`'s doc already names: a blob that was
/// edited, or one left by a handover that never completed. Refusing costs one
/// pass over at most a few dozen numbers and makes the sole-owner claim true
/// rather than merely expected.
///
/// # Errors
///
/// Names the repeated number. There is nothing to say about which of the two
/// mentions is the wrong one, because nothing here can know.
fn refuse_repeated_fds(blob: &Handover) -> io::Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for fd in blob.named_fds() {
        if !seen.insert(fd) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "the handover blob names descriptor {fd} more than once, so adopting it \
                     would build two owners of one number"
                ),
            ));
        }
    }
    Ok(())
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

/// Rebuild one sheep's six handles.
fn adopt_sheep(carried: &CarriedSheep) -> io::Result<AdoptedSheep> {
    let CarriedFds {
        out_pipe,
        err_pipe,
        out_log,
        err_log,
        stdin,
        channel,
    } = carried.fds;
    let name = &carried.name;
    Ok(AdoptedSheep {
        out_pipe: adopt_pipe(out_pipe, name, "stdout")?,
        err_pipe: adopt_pipe(err_pipe, name, "stderr")?,
        out_log: adopt_log(out_log, name, "stdout")?,
        err_log: adopt_log(err_log, name, "stderr")?,
        stdin_pipe: adopt_stdin(stdin, name)?,
        channel: adopt_channel(channel, name)?,
        carried: carried.clone(),
    })
}

/// Rebuild one shepherd channel's daemon end as an async socket, if the
/// blob named one.
///
/// # Why the kind check is `peer_addr` and not a `from_file`
///
/// [`adopt_pipe`] and [`adopt_stdin`] get theirs for free, because
/// `pipe::Receiver::from_file` and `pipe::Sender::from_file` each refuse a
/// descriptor that is not a pipe of the right direction. There is no
/// equivalent for a socket: `std::os::unix::net::UnixStream::from(OwnedFd)`
/// is infallible and checks nothing, so a number that had been closed and
/// handed to the next `open` would be adopted as a socket and written to as
/// one. `getpeername` is what refuses that, and it refuses both ways a
/// wrong number can be wrong: `ENOTSOCK` for anything that is not a socket
/// at all, and `ENOTCONN` for a socket that is listening rather than
/// connected, which is what this daemon's own control listener is.
///
/// Non-blocking is set here rather than assumed, for the reason
/// [`adopt_listener`] gives: it is a file status flag and does cross the
/// exec, but `tokio::net::UnixStream::from_std` refuses a blocking socket
/// rather than fixing one, and one `fcntl` is cheaper than depending on an
/// inherited flag.
///
/// Nothing is opened and nothing is paired again. The child's fd 3 is the
/// other end of this same socketpair and has been throughout, so a
/// successor that made a new pair would be talking to itself.
fn adopt_channel(fd: Option<RawFd>, sheep: &str) -> io::Result<Option<tokio::net::UnixStream>> {
    let Some(fd) = fd else { return Ok(None) };
    let file = adopt_fd(fd, &format!("sheep '{sheep}' shepherd channel"))?;
    let socket = std::os::unix::net::UnixStream::from(OwnedFd::from(file));
    socket.peer_addr().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("sheep '{sheep}' shepherd channel is not a connected socket: {error}"),
        )
    })?;
    socket.set_nonblocking(true)?;
    tokio::net::UnixStream::from_std(socket).map(Some)
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

/// Rebuild one stdin write end as an async writer, if the blob named one.
///
/// The mirror of [`adopt_pipe`], and the check is what makes it worth its
/// own function: `pipe::Sender::from_file` refuses a descriptor that is not
/// a pipe OR is not open for writing, so a blob that named the end the child
/// reads from is refused here rather than adopted into a `shep whisper` that
/// can never land.
///
/// Nothing is opened and nothing is reopened, exactly as everywhere else in
/// this module: the child's fd 0 is the other end of this same pipe and has
/// been throughout, so a successor that recreated the pair would be writing
/// to a pipe the app is not reading.
fn adopt_stdin(fd: Option<RawFd>, sheep: &str) -> io::Result<Option<pipe::Sender>> {
    let Some(fd) = fd else { return Ok(None) };
    let file = adopt_fd(fd, &format!("sheep '{sheep}' stdin pipe"))?;
    pipe::Sender::from_file(file).map(Some).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("sheep '{sheep}' stdin is not a writable pipe: {error}"),
        )
    })
}

/// Rebuild one log handle, if the blob named one.
///
/// Wrapped, never reopened, which is what preserves `O_APPEND`. See this
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
    use std::time::Duration;

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
            app: crate::testing::app_with("web", |_| {}).into_config(),
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
        // Bounded, as every await in `reap.rs` is. An adopted listener that
        // never became readable would otherwise hang, and a hang stops the
        // whole test binary rather than failing this one case.
        tokio::time::timeout(Duration::from_secs(10), accept)
            .await
            .expect("the adopted listener must accept")
            .unwrap()
            .expect("the adopted listener accepts");
    }

    /// A blob naming one number twice is refused before anything is adopted.
    ///
    /// Two owners of one descriptor close it twice, and the second close
    /// lands on whatever this process opened in between. That is precisely
    /// the recycling hazard `sys::adopt_handover_fd` argues cannot arise, and
    /// its argument holds only while each number has a single owner.
    ///
    /// The listener's own number is the one repeated, because the refusal has
    /// to come before the first adoption rather than at the field that
    /// happens to collide: reaching a duplicate mid-way would already have
    /// built the first owner.
    #[tokio::test]
    async fn a_blob_naming_one_descriptor_twice_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("shep.sock");
        let mut blob = blob_with(
            &socket,
            vec![carried(CarriedFds {
                out_pipe: None,
                err_pipe: None,
                out_log: None,
                err_log: None,
                stdin: None,
                channel: None,
            })],
        );
        blob.sheep[0].fds.out_log = Some(blob.listener_fd);

        let err = adopt(&blob).unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{err}");
        assert!(
            err.to_string().contains("more than once"),
            "the refusal must say what is wrong with the blob: {err}"
        );
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
                stdin: None,
                channel: None,
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
                stdin: None,
                channel: None,
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
                stdin: None,
                channel: None,
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
                stdin: None,
                channel: None,
            })],
        );

        let mut adopted = adopt(&blob).unwrap();

        let out = adopted.sheep[0].out_pipe.take().expect("an adopted pipe");
        let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(out));
        // Bounded for the reason the listener case above is: an adopted pipe
        // that produced nothing would hang the binary instead of failing here.
        let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .expect("the adopted pipe must produce the line written before it")
            .unwrap();
        assert_eq!(line.as_deref(), Some("a line"));
    }

    /// fails if a carried stdin write end does not reach the end the child
    /// reads.
    ///
    /// The direction is the whole case. Every other descriptor a sheep
    /// carries is one the daemon reads from; this is the one it writes to,
    /// and a blob that named the wrong end of the pair would still adopt,
    /// still be a pipe, and still never reach the app.
    #[tokio::test]
    async fn an_adopted_stdin_pipe_writes_to_the_end_the_child_reads() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("shep.sock");
        // The child's fd 0 stays here, exactly as it does across a real
        // exec: the daemon carries only the write end.
        let (mut child_end, daemon_end) = std::io::pipe().unwrap();
        let blob = blob_with(
            &socket,
            vec![carried(CarriedFds {
                out_pipe: None,
                err_pipe: None,
                out_log: None,
                err_log: None,
                stdin: Some(daemon_end.into_raw_fd()),
                channel: None,
            })],
        );

        let mut adopted = adopt(&blob).unwrap();

        let mut stdin = adopted.sheep[0]
            .stdin_pipe
            .take()
            .expect("an adopted stdin pipe");
        stdin.write_all(b"whisper\n").await.unwrap();
        stdin.flush().await.unwrap();
        // A blocking read of bytes already written, so there is nothing to
        // wait for and nothing to time out.
        let mut buf = [0_u8; 8];
        std::io::Read::read_exact(&mut child_end, &mut buf).expect("the child end must read");
        assert_eq!(&buf, b"whisper\n");
    }

    /// fails if the stdin number is adopted without checking which end of
    /// the pipe it is.
    ///
    /// A read end passes `is_pipe` and would be adopted as a writer, so
    /// every `shep whisper` after the handover would fail on a descriptor
    /// the successor was told was fine.
    #[tokio::test]
    async fn a_pipe_read_end_offered_as_stdin_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("shep.sock");
        let (reader, _writer) = std::io::pipe().unwrap();
        let blob = blob_with(
            &socket,
            vec![carried(CarriedFds {
                out_pipe: None,
                err_pipe: None,
                out_log: None,
                err_log: None,
                stdin: Some(reader.into_raw_fd()),
                channel: None,
            })],
        );

        let err = adopt(&blob).expect_err("a read end is not something to write to");

        let text = err.to_string();
        assert!(
            text.contains("web"),
            "the refusal must name the sheep: {text}"
        );
        assert!(
            text.contains("stdin"),
            "the refusal must name what could not be adopted: {text}"
        );
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
                stdin: None,
                channel: None,
            })],
        );

        let err = adopt(&blob).expect_err("a file is not a pipe");

        assert!(err.to_string().contains("web"), "{err}");
    }

    /// fails if an adopted shepherd channel does not still reach the same
    /// child on the same socket, in both directions.
    ///
    /// Both directions in one case rather than two, because the failure
    /// this exists to catch is one number naming the wrong end of the pair,
    /// and a case that only wrote would pass on a socket the child cannot
    /// answer. `child_end` here is what an app holds on fd 3.
    #[tokio::test]
    async fn an_adopted_channel_carries_both_directions() {
        use tokio::io::AsyncBufReadExt as _;

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("shep.sock");
        let (daemon_end, child_end) = std::os::unix::net::UnixStream::pair().unwrap();
        child_end.set_nonblocking(true).unwrap();
        let child_end = tokio::net::UnixStream::from_std(child_end).unwrap();
        let blob = blob_with(
            &socket,
            vec![carried(CarriedFds {
                out_pipe: None,
                err_pipe: None,
                out_log: None,
                err_log: None,
                stdin: None,
                channel: Some(daemon_end.into_raw_fd()),
            })],
        );

        let mut adopted = adopt(&blob).unwrap();
        let channel = adopted.sheep[0]
            .channel
            .take()
            .expect("an adopted shepherd channel");
        let (read_half, mut write_half) = tokio::io::split(channel);

        // Shepherd to child, which is what `shutdown_with_message` and a
        // `shep trigger` both ride.
        let (child_read, mut child_write) = tokio::io::split(child_end);
        let mut child = tokio::io::BufReader::new(child_read);
        write_half
            .write_all(b"{\"kind\":\"shutdown\"}\n")
            .await
            .unwrap();
        let mut line = String::new();
        child
            .read_line(&mut line)
            .await
            .expect("the child end must read");
        assert_eq!(line, "{\"kind\":\"shutdown\"}\n");

        // Child to shepherd, which is what `{"kind":"ready"}` and every
        // action reply ride.
        child_write
            .write_all(b"{\"kind\":\"ready\"}\n")
            .await
            .unwrap();
        let mut back = String::new();
        tokio::io::BufReader::new(read_half)
            .read_line(&mut back)
            .await
            .expect("the daemon end must read");
        assert_eq!(back, "{\"kind\":\"ready\"}\n");
    }

    /// fails if a channel number is adopted without checking that it still
    /// names a connected socket.
    ///
    /// `UnixStream::from(OwnedFd)` is infallible and checks nothing, unlike
    /// the two `from_file` constructors the pipes go through, so a number
    /// that had been closed and handed to the next `open` would be adopted
    /// as a socket and written to as one. A plain file is the cheapest
    /// stand-in for that.
    #[tokio::test]
    async fn a_file_offered_as_a_shepherd_channel_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("shep.sock");
        let file = tempfile::tempfile().unwrap();
        let blob = blob_with(
            &socket,
            vec![carried(CarriedFds {
                out_pipe: None,
                err_pipe: None,
                out_log: None,
                err_log: None,
                stdin: None,
                channel: Some(file.into_raw_fd()),
            })],
        );

        let err = adopt(&blob).expect_err("a file is not a connected socket");

        let text = err.to_string();
        assert!(
            text.contains("web"),
            "the refusal must name the sheep: {text}"
        );
        assert!(
            text.contains("shepherd channel"),
            "the refusal must name what could not be adopted: {text}"
        );
    }

    /// fails if a LISTENING socket offered as a channel is adopted.
    ///
    /// The one number in a blob that really is a socket and really is not a
    /// channel is this daemon's own control listener, so this is the wrong
    /// number the kind check most plausibly meets. `getpeername` answers
    /// `ENOTCONN` for it, which a check that only asked "is it a socket"
    /// would miss.
    #[tokio::test]
    async fn a_listening_socket_offered_as_a_shepherd_channel_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("shep.sock");
        let other = dir.path().join("other.sock");
        let listening = std::os::unix::net::UnixListener::bind(&other).unwrap();
        let blob = blob_with(
            &socket,
            vec![carried(CarriedFds {
                out_pipe: None,
                err_pipe: None,
                out_log: None,
                err_log: None,
                stdin: None,
                channel: Some(listening.into_raw_fd()),
            })],
        );

        let err = adopt(&blob).expect_err("a listener is not a connected socket");

        assert!(
            err.to_string().contains("shepherd channel"),
            "the refusal must name what could not be adopted: {err}"
        );
    }
}
