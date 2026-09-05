//! Rebuilding a successor's Rust-side objects around descriptors it did not
//! open.
//!
//! Every descriptor named here crossed an `execve`: the predecessor cleared
//! `FD_CLOEXEC` on it (see [`super::fds`]) and wrote its number into the
//! blob. Nothing here opens a file, binds a socket or creates a pipe.
//! `O_APPEND` and the pidfile's `flock` are properties of the open file
//! description, so wrapping keeps both: reopening a log would leave a
//! `copytruncate` rotator a sparse hole, and re-acquiring the lock would open
//! a window for a second daemon to win this home. A descriptor the blob names
//! and the process does not have refuses the whole rehydrate. The pidfile is
//! adopted last, so an earlier refusal leaves it open, unowned, and locked.

use std::fs::File;
use std::io;
use std::os::fd::{OwnedFd, RawFd};

use tokio::net::unix::pipe;

use super::{CarriedFds, CarriedSheep, Handover, SheepFd, fds};
use crate::sys;

/// Everything a successor was handed, rebuilt into objects it can use.
///
/// `Debug` is derived: descriptor numbers, a socket, a sheep's name and pid,
/// and no environment value (see [`Handover`]).
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
/// `None` on any of the first four means an instance that is registered and
/// not running, the only reason a blob names no descriptor. A descriptor
/// named and missing is a refusal, not a `None`.
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
    /// The only handle here the daemon writes to rather than reads from.
    /// `None` for the commoner sheep that has `/dev/null` on fd 0.
    pub stdin_pipe: Option<pipe::Sender>,
    /// The daemon's end of its shepherd-channel socketpair, whose other end
    /// is the child's fd 3.
    ///
    /// The only handle here that goes both ways: the successor splits it
    /// into the same reader and writer a spawn wires. `None` for a sheep
    /// whose app asked for no channel, one that is not running, and one whose
    /// child has already closed fd 3.
    pub channel: Option<tokio::net::UnixStream>,
}

/// Rebuild everything `blob` describes, around descriptors this process
/// inherited rather than opened.
///
/// # Errors
///
/// A descriptor the blob names twice, is not open, is the wrong kind for
/// its slot, or could not register with the runtime, naming the sheep and
/// the stream. No partial success: a caller that cannot rehydrate refuses to boot.
///
/// # Panics
///
/// Panics if called outside a tokio runtime with IO enabled.
#[track_caller]
pub fn adopt(blob: &Handover) -> io::Result<Adopted> {
    refuse_repeated_fds(blob)?;
    let listener = adopt_listener(blob.listener_fd)?;
    let sheep = blob
        .sheep
        .iter()
        .map(adopt_sheep)
        .collect::<io::Result<Vec<_>>>()?;
    // Last: an earlier refusal leaves this descriptor open and unowned, so
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
/// Before the first adoption, never during. `sys::adopt_handover_fd`'s
/// safety argument rests on each adoption being its number's only owner: a
/// second owner closes it again on drop, reaching whatever this process
/// opened in between. A blob the daemon wrote cannot repeat a number; this
/// covers one that was edited, or left by a handover that never completed.
///
/// # Errors
///
/// Names the repeated number, which is all anything here can know.
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

/// Run everything [`adopt`] will run, in the predecessor, while there is
/// still an image to refuse back to. No check is re-stated: each number
/// reaches the successor's own adoption, on a duplicate, from a reparsed blob.
///
/// # Errors
///
/// The blob does not parse as a successor would, or names a number that is
/// repeated, reserved, closed, or the wrong kind for its slot.
///
/// # Panics
///
/// Panics if called outside a tokio runtime with IO enabled.
#[track_caller]
pub fn dry_run(blob: &Handover) -> io::Result<()> {
    let value = serde_json::to_value(blob).map_err(io::Error::other)?;
    let blob = Handover::load_value(value).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a successor could not have read this blob back: {error}"),
        )
    })?;

    // `adopt`'s own order, so a rehearsal that stops early stops where the
    // successor would.
    refuse_repeated_fds(&blob)?;
    rehearse(blob.listener_fd, "the control listener", adopt_listener)?;
    for carried in &blob.sheep {
        let name = &carried.name;
        for (fd, slot) in carried.fds.all_kinded() {
            let Some(fd) = fd else { continue };
            rehearse(
                fd,
                &format!("sheep '{name}' {}", slot.describe()),
                |dup| match slot {
                    SheepFd::OutPipe => adopt_pipe(Some(dup), name, "stdout").map(drop),
                    SheepFd::ErrPipe => adopt_pipe(Some(dup), name, "stderr").map(drop),
                    SheepFd::OutLog => adopt_log(Some(dup), name, "stdout").map(drop),
                    SheepFd::ErrLog => adopt_log(Some(dup), name, "stderr").map(drop),
                    SheepFd::Stdin => adopt_stdin(Some(dup), name).map(drop),
                    SheepFd::Channel => adopt_channel(Some(dup), name).map(drop),
                },
            )?;
        }
    }
    rehearse(blob.pidfile_fd, "the pidfile lock", |dup| {
        adopt_fd(dup, "the pidfile lock").map(drop)
    })?;
    Ok(())
}

/// Hand `adopt_one` a duplicate of `fd`, so an adoption that takes ownership
/// can be run against a descriptor this process must keep.
///
/// # Errors
///
/// `fd` is reserved or not open, it could not be duplicated, or the adoption
/// refused the duplicate.
fn rehearse<T>(
    fd: RawFd,
    what: &str,
    adopt_one: impl FnOnce(RawFd) -> io::Result<T>,
) -> io::Result<()> {
    // The blob's number, never the duplicate's: a duplicate is always open
    // and above the floor, so a rehearsal that only saw duplicates would wave
    // through exactly the two blobs the successor is certain to refuse.
    // Labelled, because `sys::adoptable_fd` names only the number.
    sys::adoptable_fd(fd)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, format!("{what}: {error}")))?;
    let duplicate = fds::duplicate_raw(fd)?;
    // `adopt_one` owns `duplicate` on both arms, so nothing leaks: the one
    // arm that returns without consuming is `sys::adopt_handover_fd`
    // refusing, which the check above rules out for a number just created
    // above the floor.
    adopt_one(duplicate).map(drop)
}

/// Remove the blob at `path`, now that its descriptors are adopted.
///
/// Called only after [`adopt`] has succeeded: a blob left after a refusal is
/// evidence an operator can read, while one left after a success would be
/// adopted again by the next boot. A failure to remove it is logged rather
/// than returned, the flock being rehydrated already.
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
/// Non-blocking is set rather than assumed. It is a file status flag and does
/// survive the exec, but `tokio::net::UnixListener::from_std` refuses a
/// blocking socket rather than fixing one.
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
/// The kind check is `peer_addr`, since
/// `std::os::unix::net::UnixStream::from(OwnedFd)` is infallible and checks
/// nothing: `ENOTSOCK` for anything that is not a socket, `ENOTCONN` for one
/// that is listening rather than connected, which this daemon's own control
/// listener is. Non-blocking is set rather than assumed, as in
/// [`adopt_listener`]. Nothing is paired again: the child's fd 3 is the other
/// end of this same socketpair.
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
/// open for reading and sets non-blocking itself, so a blob that crossed two
/// numbers is refused rather than pumped from a file that never yields a
/// line.
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
/// `pipe::Sender::from_file` refuses a descriptor that is not a pipe or is
/// not open for writing, so a blob naming the end the child reads from is
/// refused rather than adopted into a `shep whisper` that can never land.
/// Nothing is repaired: the child's fd 0 is the other end of this same pipe.
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
/// Wrapped, never reopened, which is what preserves `O_APPEND`.
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

    use super::{adopt, dry_run, fds, io};
    use crate::handover::{CarriedFds, CarriedSheep, Handover, SheepFd, VERSION};
    use crate::privilege::SpawnIdentity;
    use shep_core::status::ProcStatus;

    /// One carried sheep named `web`, whose descriptors are `fds`.
    fn carried(fds: CarriedFds) -> CarriedSheep {
        carried_slot(0, fds)
    }

    /// [`carried`], for a named instance slot of `web`.
    ///
    /// The id and the pid move with the slot, so two of these describe two
    /// instances of one app rather than the same one twice.
    fn carried_slot(instance: u32, fds: CarriedFds) -> CarriedSheep {
        CarriedSheep {
            id: instance + 1,
            name: "web".to_owned(),
            instance,
            pid: Some(u32::from(100 + u16::try_from(instance).unwrap())),
            restarts: 0,
            epoch: 7,
            status: ProcStatus::Online,
            last_exit: None,
            credentials: SpawnIdentity::Resolved(None),
            fds,
            pending_delete: Some(false),
            manual: None,
            reload: Some(crate::entry::ReloadState::None),
            dog: None,
            pending: None,
            pending_reidentifies: Some(false),
            ready_failed: Some(false),
            restart_due: None,
            app: crate::testing::app_with("web", |_| {}).into_config(),
        }
    }

    /// A blob naming a real listener bound at `socket`, a real pidfile, and
    /// `sheep`.
    ///
    /// Both are real: `adopt` refuses a blob whose listener or pidfile is
    /// not open, so there is no such thing as a test blob without them.
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
            reloads: Some(Vec::new()),
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
        // Bounded: an adopted listener that never became readable would
        // hang the whole test binary rather than failing this case.
        tokio::time::timeout(Duration::from_secs(10), accept)
            .await
            .expect("the adopted listener must accept")
            .unwrap()
            .expect("the adopted listener accepts");
    }

    /// A blob naming one number twice is refused before anything is adopted.
    ///
    /// Two owners close one descriptor twice, and the second close lands on
    /// whatever this process opened in between. The listener's own number is
    /// the one repeated, since reaching a duplicate mid-way would already
    /// have built the first owner.
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

    /// `merge_logs` points every instance at one path, and
    /// [`refuse_repeated_fds`] refuses a blob naming any number twice. Each
    /// instance's pump runs its own `open_append`, so one inode is reached
    /// through two descriptions with two numbers; the interleaved file proves
    /// they were independent rather than merely distinct.
    #[tokio::test]
    async fn two_instances_sharing_one_log_file_are_both_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("shep.sock");
        // One path for both slots: `assemble` drops the `-<instance>`
        // suffix under `merge_logs`.
        let merged = dir.path().join("web-out.log");
        let zero = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&merged)
            .unwrap();
        let one = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&merged)
            .unwrap();
        let (zero_fd, one_fd) = (zero.into_raw_fd(), one.into_raw_fd());
        assert_ne!(
            zero_fd, one_fd,
            "two `open`s on one path must yield two numbers, or the premise \
             of this whole case is wrong"
        );
        let blob = blob_with(
            &socket,
            vec![
                carried_slot(
                    0,
                    CarriedFds {
                        out_pipe: None,
                        err_pipe: None,
                        out_log: Some(zero_fd),
                        err_log: None,
                        stdin: None,
                        channel: None,
                    },
                ),
                carried_slot(
                    1,
                    CarriedFds {
                        out_pipe: None,
                        err_pipe: None,
                        out_log: Some(one_fd),
                        err_log: None,
                        stdin: None,
                        channel: None,
                    },
                ),
            ],
        );

        let mut adopted = adopt(&blob).expect("a merged-log clustered app must be adoptable");

        assert_eq!(adopted.sheep.len(), 2, "one adopted sheep per instance");
        let mut zero = adopted.sheep[0].out_log.take().expect("slot 0's log");
        let mut one = adopted.sheep[1].out_log.take().expect("slot 1's log");
        // Alternated, so a second handle that had become an alias of the
        // first shows up as lost text rather than two clean halves. Flushed
        // per line because a `tokio::fs::File` hands the real `write(2)` to
        // the blocking pool, which finishes in its own order.
        for line in ["zero-1\n", "one-1\n", "zero-2\n", "one-2\n"] {
            let handle = if line.starts_with("zero") {
                &mut zero
            } else {
                &mut one
            };
            handle.write_all(line.as_bytes()).await.unwrap();
            handle.flush().await.unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(&merged).unwrap(),
            "zero-1\none-1\nzero-2\none-2\n",
            "both instances append into the one file, in the order written"
        );
    }

    #[tokio::test]
    async fn a_blob_naming_a_descriptor_that_is_not_open_fails_loudly() {
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
        // The pidfile is adopted last, so a failure before it leaves that
        // descriptor open and unowned, and its `flock` held.
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
        // Bounded: an adopted pipe that produced nothing would hang the
        // binary instead of failing here.
        let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .expect("the adopted pipe must produce the line written before it")
            .unwrap();
        assert_eq!(line.as_deref(), Some("a line"));
    }

    /// The direction is the whole case: every other descriptor a sheep
    /// carries is one the daemon reads from, and a blob naming the wrong end
    /// of this pair would still adopt and still be a pipe.
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

    /// A read end passes `is_pipe` and would be adopted as a writer, so every
    /// `shep whisper` after the handover would fail on a descriptor the
    /// successor was told was fine.
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
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("shep.sock");
        // `into_raw_fd`, because `adopt` takes ownership of whatever the blob
        // names: a `File` owning the same number would close it twice.
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

    /// Both directions in one case, because the failure to catch is a number
    /// naming the wrong end of the pair and a case that only wrote would pass
    /// on a socket the child cannot answer. `child_end` is what an app holds
    /// on fd 3.
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

    /// `UnixStream::from(OwnedFd)` is infallible and checks nothing, unlike
    /// the `from_file` constructors the pipes go through, so a number handed
    /// to the next `open` would be adopted as a socket. A plain file is the
    /// cheapest stand-in.
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

    /// This daemon's own control listener is the one number in a blob that is
    /// a socket and is not a channel, so it is the wrong number the kind check
    /// most plausibly meets. `getpeername` answers `ENOTCONN`, which a check
    /// asking only "is it a socket" would miss.
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

    /// A predecessor's live descriptors: one of everything a blob names,
    /// still owned here rather than leaked into a number.
    ///
    /// [`blob_with`] hands its listener and pidfile to `into_raw_fd`, which
    /// suits a case about `adopt`. `dry_run`'s contract is that the caller
    /// still owns everything afterwards, which nothing can check against
    /// numbers no value holds.
    struct Predecessor {
        dir: tempfile::TempDir,
        listener: std::os::unix::net::UnixListener,
        pidfile: std::fs::File,
        out_log: std::fs::File,
        out_read: std::io::PipeReader,
        out_write: std::io::PipeWriter,
        stdin_read: std::io::PipeReader,
        stdin_write: std::io::PipeWriter,
        channel: std::os::unix::net::UnixStream,
        child_channel: std::os::unix::net::UnixStream,
    }

    impl Predecessor {
        /// One of each kind, all open, all the right way round.
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let (out_read, out_write) = std::io::pipe().unwrap();
            let (stdin_read, stdin_write) = std::io::pipe().unwrap();
            let (channel, child_channel) = std::os::unix::net::UnixStream::pair().unwrap();
            Self {
                listener: std::os::unix::net::UnixListener::bind(dir.path().join("shep.sock"))
                    .unwrap(),
                pidfile: tempfile::tempfile().unwrap(),
                out_log: tempfile::tempfile().unwrap(),
                out_read,
                out_write,
                stdin_read,
                stdin_write,
                channel,
                child_channel,
                dir,
            }
        }

        /// Where this fixture's listener is bound.
        fn socket(&self) -> std::path::PathBuf {
            self.dir.path().join("shep.sock")
        }

        /// A blob naming every one of them, for one sheep called `web`.
        fn blob(&self) -> Handover {
            use std::os::fd::AsRawFd as _;
            Handover {
                version: VERSION,
                sheep: vec![carried(CarriedFds {
                    out_pipe: Some(self.out_read.as_raw_fd()),
                    err_pipe: None,
                    out_log: Some(self.out_log.as_raw_fd()),
                    err_log: None,
                    stdin: Some(self.stdin_write.as_raw_fd()),
                    channel: Some(self.channel.as_raw_fd()),
                })],
                listener_fd: self.listener.as_raw_fd(),
                pidfile_fd: self.pidfile.as_raw_fd(),
                next_id: 9,
                next_deadline: 5,
                next_action_stamp: 2,
                reloads: Some(Vec::new()),
            }
        }
    }

    /// How many descriptors this process holds, counted the only way a
    /// portable test can: by asking about every number up to a bound.
    ///
    /// The bound is generous rather than exact: this is used as a before and
    /// after pair, so what matters is that the same numbers are asked about
    /// both times.
    fn open_fd_count() -> usize {
        (0..512)
            .filter(|fd| crate::sys::adoptable_fd(*fd).is_ok())
            .count()
    }

    /// Run the real [`adopt`] over DUPLICATES of `blob`'s descriptors.
    ///
    /// `adopt` takes ownership, so a case run against a [`Predecessor`]'s own
    /// numbers would close the fixture. Duplicating renumbers, so a blob
    /// naming one descriptor twice stops naming it twice here.
    ///
    /// Closes every duplicate it holds, unless [`Self::release`] is called
    /// first: `fds::duplicate_raw` hands back a bare number with no owner,
    /// and a `Handover` holding it closes nothing on drop.
    struct Duplicates(Vec<RawFd>);

    impl Duplicates {
        fn of(&mut self, fd: RawFd) -> io::Result<RawFd> {
            let duplicate = fds::duplicate_raw(fd)?;
            self.0.push(duplicate);
            Ok(duplicate)
        }

        fn release(mut self) {
            self.0.clear();
        }
    }

    impl Drop for Duplicates {
        fn drop(&mut self) {
            for fd in self.0.drain(..) {
                let _ = nix::unistd::close(fd);
            }
        }
    }

    fn adopt_a_copy(blob: &Handover) -> io::Result<()> {
        // Propagated, never `unwrap_or(fd)`: falling back to the original
        // hands `adopt` the fixture's own live descriptor to close.
        let mut dups = Duplicates(Vec::new());
        let mut copy = blob.clone();
        copy.listener_fd = dups.of(copy.listener_fd)?;
        copy.pidfile_fd = dups.of(copy.pidfile_fd)?;
        for sheep in &mut copy.sheep {
            sheep.fds = CarriedFds {
                out_pipe: sheep.fds.out_pipe.map(|fd| dups.of(fd)).transpose()?,
                err_pipe: sheep.fds.err_pipe.map(|fd| dups.of(fd)).transpose()?,
                out_log: sheep.fds.out_log.map(|fd| dups.of(fd)).transpose()?,
                err_log: sheep.fds.err_log.map(|fd| dups.of(fd)).transpose()?,
                stdin: sheep.fds.stdin.map(|fd| dups.of(fd)).transpose()?,
                channel: sheep.fds.channel.map(|fd| dups.of(fd)).transpose()?,
            };
        }
        // Released before the fallible call: `adopt` consumes the numbers it
        // reaches and says nothing about which, so holding the guard across it
        // would close some twice. A leak bounded by a test binary is better.
        dups.release();
        adopt(&copy).map(drop)
    }

    /// Without this the suite would pass with a `dry_run` that refused
    /// everything, turning every handover into a stop-and-start.
    #[tokio::test]
    async fn a_blob_a_successor_could_adopt_passes_the_rehearsal() {
        let predecessor = Predecessor::new();

        dry_run(&predecessor.blob()).expect("every descriptor here is the kind its slot wants");
    }

    /// Nothing about a carried reload is a descriptor, so the rehearsal
    /// covers a swap in flight through the parse alone,
    /// [`Handover::load_value`] running before any descriptor is touched.
    #[tokio::test]
    async fn a_blob_carrying_a_swap_in_flight_passes_the_rehearsal() {
        use crate::entry::ReloadState;
        use crate::supervisor::{CarriedReload, ReloadMode, ReloadPhase, ReloadSwap};

        let predecessor = Predecessor::new();
        let mut blob = predecessor.blob();
        blob.sheep[0].reload = Some(ReloadState::Drainee { new_id: Some(9) });
        blob.reloads = Some(vec![CarriedReload {
            app: "web".to_owned(),
            queue: vec![4, 5],
            mode: ReloadMode::Overlap,
            swap: ReloadSwap {
                old_id: 1,
                new_id: Some(9),
                phase: ReloadPhase::DrainOld,
            },
        }]);

        dry_run(&blob).expect("a flock mid-reload is one a successor can adopt");
    }

    /// A field added to [`CarriedSheep`] rides the reparse the rehearsal
    /// already runs, and one it could not parse would be found after the
    /// predecessor was gone.
    #[tokio::test]
    async fn a_blob_carrying_a_failed_readiness_verdict_passes_the_rehearsal() {
        let predecessor = Predecessor::new();
        let mut blob = predecessor.blob();
        blob.sheep[0].ready_failed = Some(true);

        dry_run(&blob).expect("an instance a reload gave up on is one a successor can adopt");
    }

    /// The predecessor is still supervising this flock, so everything it
    /// holds has to work afterwards. Each of the four takes ownership a
    /// different way and would close the fixture's handle without the
    /// duplicate.
    ///
    /// The connection is queued before the rehearsal, since `O_NONBLOCK`
    /// reaches the original through the duplicate and this fixture's listener
    /// is a plain `std` one that really does change. Queueing first leaves
    /// the accept below something waiting either way.
    #[tokio::test]
    async fn a_rehearsal_leaves_every_descriptor_it_checked_working() {
        use std::io::{Read as _, Write as _};

        let mut predecessor = Predecessor::new();
        predecessor.out_write.write_all(b"a bleat").unwrap();
        let socket = predecessor.socket();
        let connecting = tokio::task::spawn_blocking(move || {
            std::os::unix::net::UnixStream::connect(&socket).unwrap()
        });
        let client = connecting.await.unwrap();

        dry_run(&predecessor.blob()).expect("the fixture is adoptable");

        // The listener still listens.
        predecessor
            .listener
            .accept()
            .expect("the checked listener still accepts");
        drop(client);

        // The stdout pipe still carries what was written before the check.
        let mut buf = [0_u8; 7];
        predecessor
            .out_read
            .read_exact(&mut buf)
            .expect("the checked read end still reads");
        assert_eq!(&buf, b"a bleat");

        // The stdin pipe still carries a line the other way.
        predecessor.stdin_write.write_all(b"whisper").unwrap();
        let mut buf = [0_u8; 7];
        predecessor
            .stdin_read
            .read_exact(&mut buf)
            .expect("the checked write end still writes");
        assert_eq!(&buf, b"whisper");

        // The shepherd channel still has its child on the far end.
        predecessor.channel.write_all(b"ping").unwrap();
        let mut buf = [0_u8; 4];
        predecessor
            .child_channel
            .read_exact(&mut buf)
            .expect("the checked channel still reaches the child");
        assert_eq!(&buf, b"ping");
    }

    /// A duplicate taken and never handed to an adoption leaks one
    /// descriptor per named number, on a path that runs on every reload.
    ///
    /// Counted over a hundred passes, because the count is the whole
    /// process's and other cases run in other threads. Six hundred leaked
    /// descriptors sit well clear of a few dozen of concurrent noise.
    #[tokio::test]
    async fn a_rehearsal_leaks_no_descriptors() {
        let predecessor = Predecessor::new();
        let blob = predecessor.blob();
        let before = open_fd_count();

        for _ in 0..100 {
            dry_run(&blob).expect("the fixture is adoptable");
        }

        let after = open_fd_count();
        assert!(
            after < before + 100,
            "a hundred rehearsals of a blob naming six descriptors must not grow this \
             process's descriptor table: {before} -> {after}"
        );
    }

    /// A closed number is already refused before the exec, since clearing
    /// `FD_CLOEXEC` on it meets `EBADF`. An open one of the wrong kind sails
    /// through that and would reach a successor with no predecessor left.
    #[tokio::test]
    async fn a_descriptor_that_is_open_but_not_a_pipe_is_refused_before_the_exec() {
        use std::os::fd::AsRawFd as _;

        let predecessor = Predecessor::new();
        let not_a_pipe = std::fs::File::open("/dev/null").unwrap();
        let mut blob = predecessor.blob();
        blob.sheep[0].fds.out_pipe = Some(not_a_pipe.as_raw_fd());

        // The premise: this number is open, so nothing before the exec
        // would have stopped it.
        crate::sys::adoptable_fd(not_a_pipe.as_raw_fd())
            .expect("the number must be open, or this proves nothing");
        fds::keep_raw_across_exec(not_a_pipe.as_raw_fd())
            .expect("the `FD_CLOEXEC` sweep must not refuse it either");

        let err = dry_run(&blob).expect_err("/dev/null is not a readable pipe");

        assert!(
            err.to_string().contains("web") && err.to_string().contains("stdout"),
            "the refusal must name the sheep and the stream: {err}"
        );
    }

    /// The rehearsal and the adoption must agree, slot by slot.
    ///
    /// A rehearsal that passes a blob the successor refuses still reaches
    /// the `execve`. Every slot is walked because four mechanisms refuse
    /// them: a readable-pipe check, a writable-pipe check, a `getpeername`,
    /// and for the two log slots no kind check at all.
    #[tokio::test]
    async fn the_rehearsal_and_the_adoption_agree_on_every_slot() {
        use std::os::fd::AsRawFd as _;

        for slot in [
            SheepFd::OutPipe,
            SheepFd::ErrPipe,
            SheepFd::OutLog,
            SheepFd::ErrLog,
            SheepFd::Stdin,
            SheepFd::Channel,
        ] {
            let predecessor = Predecessor::new();
            let wrong = std::fs::File::open("/dev/null").unwrap();
            let wrong = Some(wrong.as_raw_fd());
            let mut blob = predecessor.blob();
            let fds = &mut blob.sheep[0].fds;
            match slot {
                SheepFd::OutPipe => fds.out_pipe = wrong,
                SheepFd::ErrPipe => fds.err_pipe = wrong,
                SheepFd::OutLog => fds.out_log = wrong,
                SheepFd::ErrLog => fds.err_log = wrong,
                SheepFd::Stdin => fds.stdin = wrong,
                SheepFd::Channel => fds.channel = wrong,
            }

            assert_eq!(
                dry_run(&blob).is_err(),
                adopt_a_copy(&blob).is_err(),
                "the rehearsal and the adoption disagree about {slot:?}, so one of them is \
                 checking something the other is not"
            );
        }
    }

    /// A repeated number is refused by the same function the successor
    /// refuses it with, rather than by a second copy of the rule.
    ///
    /// The sweep before the exec cannot catch it: clearing `FD_CLOEXEC`
    /// twice on one number succeeds, so a repeat reaches the successor
    /// untouched.
    #[tokio::test]
    async fn a_blob_naming_one_descriptor_twice_is_refused_before_the_exec() {
        let predecessor = Predecessor::new();
        let mut blob = predecessor.blob();
        blob.sheep[0].fds.err_log = Some(blob.pidfile_fd);

        let err = dry_run(&blob).expect_err("one number cannot have two owners");

        assert!(
            err.to_string().contains("more than once"),
            "the refusal must be `refuse_repeated_fds`'s own: {err}"
        );
    }

    /// A number below the stdio floor is refused here, not after the exec.
    ///
    /// The check runs against the blob's number, not the duplicate's: a
    /// duplicate is always open and above the floor, so a rehearsal that only
    /// inspected duplicates would wave through exactly the two blobs
    /// `sys::adopt_handover_fd` is certain to refuse.
    #[tokio::test]
    async fn a_reserved_or_closed_number_is_refused_before_the_exec() {
        let predecessor = Predecessor::new();

        let mut reserved = predecessor.blob();
        reserved.sheep[0].fds.out_log = Some(0);
        let err = dry_run(&reserved).expect_err("stdio is owned elsewhere");
        assert!(
            err.to_string().contains("reserved for stdio"),
            "the refusal must be the successor's own wording: {err}"
        );

        // `RawFd::MAX` rather than a number this case opened and closed: a
        // closed number can be handed straight back to another thread of this
        // same process. A number above any descriptor limit is `EBADF` with
        // nothing to race against.
        let mut gone = predecessor.blob();
        gone.sheep[0].fds.err_log = Some(RawFd::MAX);
        let err = dry_run(&gone).expect_err("a number this high names nothing");
        assert!(
            err.to_string().contains("not an open descriptor"),
            "the refusal must be the successor's own wording: {err}"
        );
    }

    /// The successor reads the blob back off disk rather than being handed
    /// this struct, so the parse is one of its checks too.
    ///
    /// A successor is a different build, so one that has moved `VERSION`
    /// refuses the blob at `load_value`, past the exec and with the
    /// predecessor gone.
    #[tokio::test]
    async fn a_blob_a_successor_could_not_read_back_is_refused_before_the_exec() {
        let predecessor = Predecessor::new();
        let mut blob = predecessor.blob();
        blob.version = VERSION + 1;

        let err = dry_run(&blob).expect_err("a version this image cannot read");

        assert!(
            err.to_string()
                .contains("could not have read this blob back"),
            "the refusal must say the parse failed, not the descriptors: {err}"
        );
    }
}
