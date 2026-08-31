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

use super::{CarriedFds, CarriedSheep, Handover, SheepFd, fds};
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

/// Run everything [`adopt`] will run, in the PREDECESSOR, while there is
/// still an image to refuse back to.
///
/// # Why this exists at all
///
/// The `execve` is one-way. Nothing re-execs a predecessor, and its image is
/// gone the instant the successor starts, so a successor that cannot adopt
/// its blob has no way back: `boot::rehydrate` returns `BootError::Adopt`,
/// the daemon exits without ever serving, and the flock it was handed keeps
/// running with nothing supervising it. The design called that case
/// "rollback"; there is nothing to roll back to, and this is the honest
/// shape of the same intent. Check first, exec second, and a failure becomes
/// the ordinary stop-and-start reload rather than an unsupervised flock.
///
/// # Why it runs the real adoption instead of describing one
///
/// A second implementation of "is this descriptor adoptable" is worse than
/// none. If the successor's checks tighten and a hand-written copy here does
/// not, the rehearsal passes, the exec happens, and the boot still fails —
/// with the predecessor now gone. So nothing here re-states a check: this
/// walks [`CarriedFds::all_kinded`], the one place a number is paired with
/// its slot, and hands each number to the SAME function the successor will
/// hand it to. [`refuse_repeated_fds`] is called rather than re-derived, and
/// [`sys::adoptable_fd`] is [`sys::adopt_handover_fd`]'s own two checks,
/// shared rather than copied.
///
/// The parse is part of it too. A successor does not receive this struct; it
/// reads bytes back off disk and rebuilds one, so the version gate and the
/// deserialize are checks it makes as surely as the descriptors are. This
/// runs them through [`Handover::load_value`], the successor's own entry
/// point, and then rehearses the adoption against the REPARSED blob rather
/// than against the one it was handed.
///
/// One seam is not shared, and it is worth naming rather than hiding: the
/// successor reads bytes with `serde_json::from_str` while this goes through
/// `serde_json::to_value`. Anything that survives one and not the other
/// would slip past. The gap is the JSON text itself, and `Handover::write`
/// has already proved that serializes.
///
/// # What it does to the descriptors it checks
///
/// It never takes one. Every adoption here is handed a duplicate
/// ([`fds::duplicate_raw`]), takes ownership of THAT, and closes it when the
/// value is dropped at the end of the call. Ownership is what matters,
/// because this runs while the predecessor is still supervising the flock:
/// the pumps are parked, not shut down, and a refusal resumes them and
/// stops gracefully.
///
/// One thing does reach the original, and it is worth naming rather than
/// claiming a clean sweep. [`adopt_listener`], [`adopt_pipe`],
/// [`adopt_stdin`] and [`adopt_channel`] each set `O_NONBLOCK`, which is a
/// property of the open file description rather than of the descriptor, so
/// a duplicate does not insulate the original from it. In this daemon that
/// is a write of the value already there: every one of those is a tokio
/// object, and tokio does not accept a blocking one — `UnixListener::
/// from_std` and `UnixStream::from_std` refuse it outright, and the two
/// `pipe` constructors set it themselves. So the flag this could change is
/// one nothing here holds unset.
///
/// # Errors
///
/// The blob does not parse as a successor would parse it, it names one
/// descriptor number twice, or a number it names is reserved, is not open,
/// or is not the kind of object its slot will be adopted as. The message is
/// the successor's own, so it names the sheep and the stream.
///
/// # Panics
///
/// Panics if called outside a tokio runtime with IO enabled, exactly as
/// [`adopt`] does and for the same reason: the objects it builds register
/// with the runtime's reactor, which has nowhere to happen without one.
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
    // successor would: repeats first, then the listener, then each sheep,
    // then the pidfile.
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
    // The BLOB's number is checked here, never the duplicate's, and the
    // distinction is the whole reason this is not one line shorter.
    // `sys::adopt_handover_fd` refuses a number below 3 or one that is not
    // open, and a duplicate is always neither — so a rehearsal that only
    // ever saw duplicates would wave through exactly the two blobs the
    // successor is certain to refuse.
    // Labelled, because `sys::adoptable_fd` names only the NUMBER. A blob
    // carries six descriptors per sheep, so `fd 7 is not an open descriptor`
    // is a refusal an operator cannot map back to a stream. `dry_run`'s doc
    // promises the message names the sheep and the stream, and this arm is
    // the one that would otherwise make that false.
    sys::adoptable_fd(fd)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, format!("{what}: {error}")))?;
    let duplicate = fds::duplicate_raw(fd)?;
    // `adopt_one` owns `duplicate` from here, on BOTH arms, and that is what
    // makes this leak nothing. Every adoption in this module either builds
    // an owner of the number or drops the `File` it had already built; the
    // one arm that returns without consuming is `sys::adopt_handover_fd`
    // refusing, and the check above is what rules that out for a number this
    // call just created above the floor.
    adopt_one(duplicate).map(drop)
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
    /// real instances of one app rather than the same one twice.
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
            ready_failed: Some(false),
            restart_due: None,
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

    /// Fails if `merge_logs` makes a two-instance app unadoptable, or if
    /// the two handles it carries stop being independent across the exec.
    ///
    /// The hazard this closes is [`refuse_repeated_fds`], which refuses the
    /// WHOLE blob when any descriptor number appears twice. `merge_logs`
    /// points every instance of an app at one path, and if that were one
    /// open file description shared between them then every merged
    /// clustered app would be refused forever, with a message blaming a
    /// descriptor rather than the config that produced it.
    ///
    /// It is not one description. Each instance's pump runs its own
    /// `open_append` on the path (`tokio_runner`'s `LogFile::open`), so one
    /// inode is reached through two descriptions with two numbers, and
    /// `O_APPEND` is what keeps their writes from overwriting each other.
    /// The two numbers are what this asserts on, and the interleaved file
    /// afterwards is what proves they were really independent rather than
    /// merely distinct.
    #[tokio::test]
    async fn two_instances_sharing_one_log_file_are_both_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("shep.sock");
        // One path for both slots, which is exactly what `merge_logs` does:
        // `assemble` drops the `-<instance>` suffix and every instance of
        // the name lands here.
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
        // Alternated, so a second handle that had silently become the first
        // one's alias shows up as lost or overwritten text rather than as
        // two clean halves.
        //
        // Flushed after every line, and that is not tidiness. A
        // `tokio::fs::File` buffers and hands the real `write(2)` to the
        // blocking pool, so two handles written in sequence with one flush
        // at the end reach the file in whichever order that pool finished:
        // measured, this case failed once in twelve runs on
        // `zero-1/one-1/one-2/zero-2`. The flush is what makes each write
        // land before the next begins, which is what an ordered assertion
        // needs to be an assertion about `O_APPEND` rather than about a
        // thread pool.
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

    /// A predecessor's live descriptors: one of everything a blob names,
    /// still owned HERE rather than leaked into a number.
    ///
    /// [`blob_with`] above hands its listener and pidfile to
    /// `into_raw_fd`, which is right for a case about `adopt`, since
    /// `adopt` takes ownership and there is nobody left to take it from.
    /// `dry_run` is the opposite situation and needs the opposite fixture:
    /// its whole contract is that the caller still owns everything
    /// afterwards, and nothing can check that against numbers no value
    /// holds.
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
    /// The bound is generous rather than exact — this is used as a before
    /// and after pair, so what matters is that the same numbers are asked
    /// about both times, not that the total is the true one.
    fn open_fd_count() -> usize {
        (0..512)
            .filter(|fd| crate::sys::adoptable_fd(*fd).is_ok())
            .count()
    }

    /// Run the real [`adopt`] over DUPLICATES of `blob`'s descriptors.
    ///
    /// The point of the duplicates is that `adopt` takes ownership: a case
    /// that ran it against a [`Predecessor`]'s own numbers would close the
    /// fixture and then measure a closed one. Note the one thing this
    /// cannot be used to compare, because duplicating renumbers: a blob
    /// naming the same descriptor twice stops naming it twice here.
    /// Closes every duplicate it holds, unless [`Self::release`] is called
    /// first.
    ///
    /// `fds::duplicate_raw` hands back a bare number with no owner, so a `?`
    /// part way through the copy below would leak every duplicate made
    /// before the failing one. Nothing else would ever close them: the
    /// numbers live in a `Handover` as plain integers, and dropping that
    /// closes nothing.
    ///
    /// The release is not tidiness either. `adopt` takes ownership of the
    /// numbers it is handed, so this must let go before that call or each
    /// one is closed twice.
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
        // Propagated, never `unwrap_or(fd)`. Falling back to the original
        // hands `adopt` the fixture's own live descriptor, which it then
        // closes -- the exact thing the doc above says the duplicates exist
        // to prevent, reintroduced on the one path nobody watches. A case
        // that hit it would fail somewhere else entirely, measuring a
        // descriptor this helper had shut.
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
        // Released BEFORE the fallible call, not after, and the asymmetry is
        // deliberate. `adopt` consumes the numbers it reaches and drops the
        // owners it had already built when it refuses, so on its error path
        // some of these are closed and some are not, and it returns nothing
        // that says which. Holding the guard across it would close the
        // consumed ones a second time, and a double close can shut whatever
        // number the kernel has since handed out. A leak bounded by a test
        // binary is the better of those two.
        //
        // That the unreached ones leak is not this helper's invention: it is
        // `adopt`'s own documented behaviour, which is why the pidfile is
        // adopted last (see this module's header).
        dups.release();
        adopt(&copy).map(drop)
    }

    /// The happy path, and the property everything else rests on: a blob a
    /// successor could adopt is not refused here.
    ///
    /// Without this the suite could pass with a `dry_run` that refused
    /// everything, which would turn every handover into a stop-and-start
    /// and lose the feature while looking safe.
    #[tokio::test]
    async fn a_blob_a_successor_could_adopt_passes_the_rehearsal() {
        let predecessor = Predecessor::new();

        dry_run(&predecessor.blob()).expect("every descriptor here is the kind its slot wants");
    }

    /// Fails if a blob describing a flock mid-reload stops surviving the
    /// reparse the rehearsal runs it through.
    ///
    /// The rehearsal is the predecessor running the successor's OWN checks
    /// while there is still an image to refuse back to, and one of those
    /// checks is the parse: it reads the whole blob back through
    /// [`Handover::load_value`] before rehearsing a single descriptor. So a
    /// swap in flight is covered by the rehearsal for free — nothing about a
    /// carried reload is a descriptor, and `adopt` gains no refusal for one
    /// — but "for free" is a claim that has to be pinned rather than
    /// asserted, because the cost of it being wrong is the successor
    /// refusing to boot with the predecessor already gone.
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

    /// Fails if a blob carrying a failed readiness verdict cannot be
    /// rehearsed.
    ///
    /// The rehearsal reparses the whole blob through the successor's own
    /// [`Handover::load_value`] before it touches a descriptor, so a field
    /// added to [`CarriedSheep`] rides the parse it already runs. That is
    /// worth a case rather than an assumption: the rehearsal is what stands
    /// between a bad blob and an `execve` with no way back, and a field it
    /// could not parse would be found after the predecessor was gone.
    #[tokio::test]
    async fn a_blob_carrying_a_failed_readiness_verdict_passes_the_rehearsal() {
        let predecessor = Predecessor::new();
        let mut blob = predecessor.blob();
        blob.sheep[0].ready_failed = Some(true);

        dry_run(&blob).expect("an instance a reload gave up on is one a successor can adopt");
    }

    /// The whole reason the rehearsal runs against duplicates: the
    /// predecessor is still supervising this flock, and everything it holds
    /// has to work afterwards.
    ///
    /// Each of the four is a different way `adopt` takes ownership —
    /// `UnixListener::from_std`, `pipe::Receiver::from_file`,
    /// `pipe::Sender::from_file`, `UnixStream::from_std` — and each would
    /// close the fixture's own handle if the duplicate were skipped.
    ///
    /// The connection is queued BEFORE the rehearsal rather than after, and
    /// that is the one place this fixture differs from a live daemon.
    /// `adopt_listener` and `adopt_channel` each set `O_NONBLOCK`, which is
    /// a property of the open file description and so reaches the original
    /// through the duplicate. In the daemon every one of these is already
    /// non-blocking because tokio owns it, so the `fcntl` writes back what
    /// was already there; this fixture's listener is a plain `std` one and
    /// really does change. Queueing first means the accept below has
    /// something waiting either way.
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
    /// Counted over a hundred passes rather than one, because the number
    /// this can measure is the whole process's and the suite is running
    /// other cases in other threads at the same time. A hundred passes of a
    /// blob naming six descriptors leaks six hundred; the concurrent noise
    /// is a few dozen either way, so the threshold sits comfortably between
    /// them and needs no quiet machine.
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

    /// The case with no recovery: a descriptor that is open, so the
    /// `FD_CLOEXEC` sweep clears it without complaint, but is not the kind
    /// its slot will be adopted as.
    ///
    /// This is the shape the whole task exists for. A closed number is
    /// already refused before the exec, because clearing `FD_CLOEXEC` on it
    /// meets `EBADF`; an OPEN one of the wrong kind sails through that,
    /// reaches the `execve`, and is refused by a successor with no
    /// predecessor left to hand the flock back to.
    #[tokio::test]
    async fn a_descriptor_that_is_open_but_not_a_pipe_is_refused_before_the_exec() {
        use std::os::fd::AsRawFd as _;

        let predecessor = Predecessor::new();
        let not_a_pipe = std::fs::File::open("/dev/null").unwrap();
        let mut blob = predecessor.blob();
        blob.sheep[0].fds.out_pipe = Some(not_a_pipe.as_raw_fd());

        // The premise, so the case cannot pass for the wrong reason: this
        // number IS open, so nothing before the exec would have stopped it.
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
    /// The failure this guards against is specific and is worse than not
    /// rehearsing: a rehearsal that passes a blob the successor refuses
    /// still reaches the `execve`, and by then there is no way back. Every
    /// slot is walked because they are refused by four different mechanisms
    /// — a readable-pipe check, a writable-pipe check, a `getpeername`, and
    /// for the two log slots no kind check at all — and a pairing that put
    /// the wrong one against a slot would pass on a narrower case.
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
    /// It is a case the sweep before the exec cannot catch: clearing
    /// `FD_CLOEXEC` twice on one number succeeds, deliberately and
    /// idempotently, so a repeat reaches the successor untouched.
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
    /// The check runs against the BLOB's number rather than the duplicate's.
    /// A duplicate is always open and always above the floor, so a
    /// rehearsal that only ever inspected duplicates would wave through
    /// exactly the two blobs `sys::adopt_handover_fd` is certain to refuse.
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

        // `RawFd::MAX` rather than a number this case opened and closed. A
        // closed number is the honest fixture and is also a race: the suite
        // runs other cases in other threads of this same process, and one of
        // them opening a file between the close and the assertion hands that
        // number straight back. Caught once, on the unfiltered workspace run
        // and never on the filtered one. A number above any process's
        // descriptor limit is `EBADF` with nothing to race against, and the
        // property under test is the same one — that the blob names no open
        // descriptor.
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
    /// A version it cannot read is the reachable case: a handover's whole
    /// point is that the successor is a DIFFERENT build, and one that has
    /// moved `VERSION` refuses the blob at `load_value` — after the exec,
    /// with the predecessor gone.
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
