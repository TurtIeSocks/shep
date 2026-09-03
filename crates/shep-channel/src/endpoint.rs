//! Finding the channel this process was given, and opening it.
//!
//! Branch on which environment variable is present, never on the platform.
//! The daemon sets exactly one of them and never both: `SHEP_CHANNEL_FD` on
//! unix, `SHEP_CHANNEL_PIPE` on Windows. Neither means no channel was opened
//! for this process, which is the ordinary case.

use std::path::PathBuf;

use core::sync::atomic::{AtomicBool, Ordering};

use crate::ChannelError;

/// The descriptor number variable, set on unix only.
pub const FD_VAR: &str = "SHEP_CHANNEL_FD";
/// The named pipe variable, set on Windows only.
pub const PIPE_VAR: &str = "SHEP_CHANNEL_PIPE";
/// The wire version stamp, set on both platforms whenever a channel exists.
pub const VERSION_VAR: &str = "SHEP_CHANNEL_VERSION";

/// The lowest descriptor number the channel can arrive on.
///
/// The shepherd maps the child's end of the socketpair onto 3 and never
/// anything lower, because 0, 1 and 2 are the app's own standard streams.
const FIRST_INHERITABLE_FD: i32 = 3;

/// Where this process's channel is, if it has one.
///
/// # Debug
///
/// Derived, and prints the Windows pipe path in full including the random
/// suffix the shepherd puts on it. That is a decision rather than an
/// oversight, so it has a test.
///
/// The suffix is not a secret. `tokio_runner.rs`, where the shepherd builds
/// the name, argues that 128 bits closes prediction and not observation: the
/// pipe namespace lists to any unprivileged local user, measured at 190
/// pipes from a non-elevated session, so anyone positioned to read this
/// value out of a log can enumerate the live name directly. It is also in
/// `SHEP_CHANNEL_PIPE` in this process's own environment, and the instance
/// it names is consumed once the app connects.
///
/// The rest of the crate leaks nothing here to match, which is worth knowing
/// before adding a redaction: on Windows `std::fs::File`'s own `Debug` is
/// `File { handle: 0xb4 }` and carries no path, so [`crate::Channel`] and
/// the reader half print handles rather than names.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// An inherited descriptor number, from `SHEP_CHANNEL_FD`.
    Descriptor(i32),
    /// A named pipe path, from `SHEP_CHANNEL_PIPE`.
    Pipe(PathBuf),
    /// Neither variable is set. Not an error.
    Absent,
}

/// Reads the environment and says where the channel is.
///
/// # Errors
///
/// [`ChannelError::Unusable`] when `SHEP_CHANNEL_FD` is set to something
/// that is not a descriptor number, or to a number below 3. Either is a
/// broken environment rather than an absent channel, and saying so beats
/// silently doing nothing.
pub fn discover() -> Result<Endpoint, ChannelError> {
    if let Some(raw) = std::env::var_os(FD_VAR) {
        return descriptor_from(&raw.to_string_lossy());
    }
    if let Some(raw) = std::env::var_os(PIPE_VAR) {
        return Ok(Endpoint::Pipe(PathBuf::from(raw)));
    }
    Ok(Endpoint::Absent)
}

/// Reads one `SHEP_CHANNEL_FD` value and says whether it can be the channel.
///
/// Split out of [`discover`] so the refusals below can be tested without
/// setting an environment variable: that is `unsafe` in edition 2024, which
/// this crate denies outside its one descriptor site, and it would race
/// every other test sharing the process.
///
/// The floor is what [`connect`]'s `from_raw_fd` leans on. A negative number
/// is not an owned descriptor at all, and 1 is worse in practice than a
/// number that is merely wrong: taking it would hand this crate ownership of
/// the app's stdout, write JSON into it, and close it on drop.
fn descriptor_from(text: &str) -> Result<Endpoint, ChannelError> {
    let Ok(fd) = text.trim().parse::<i32>() else {
        return Err(ChannelError::Unusable(format!("{FD_VAR}={text}")));
    };
    if fd < FIRST_INHERITABLE_FD {
        return Err(ChannelError::Unusable(format!(
            "{FD_VAR}={fd} must be {FIRST_INHERITABLE_FD} or above: the shepherd passes \
             the channel as {FIRST_INHERITABLE_FD}, and 0, 1 and 2 are this process's own \
             standard streams"
        )));
    }
    Ok(Endpoint::Descriptor(fd))
}

/// The duplex this platform carries the channel on.
#[cfg(unix)]
pub(crate) type Transport = std::os::unix::net::UnixStream;
/// The duplex this platform carries the channel on.
#[cfg(windows)]
pub(crate) type Transport = std::fs::File;

/// The half the reader thread owns.
///
/// The duplex itself on unix, where a socketpair's two ends are independent
/// open file descriptions and a `read` parked on one costs a concurrent
/// `write` on the other nothing at all.
#[cfg(unix)]
pub(crate) type ReadHalf = Transport;
/// The half the reader thread owns.
///
/// [`PipeReader`] on Windows, which is not a refinement -- see that type for
/// the deadlock it exists to avoid.
#[cfg(windows)]
pub(crate) type ReadHalf = PipeReader;

/// How long the Windows reader sleeps between peeks at an empty pipe.
///
/// The price of not being able to park inside `ReadFile`. What arrives on
/// this half is an operator triggering an action or asking the app to stop,
/// measured against `action_timeout` and `kill_timeout` -- both seconds --
/// so 20 ms is invisible where a deadlock is not. Much shorter spins for
/// nothing; much longer starts to show in how quickly an app answers
/// `shep trigger`.
#[cfg(windows)]
const PIPE_POLL_INTERVAL: core::time::Duration = core::time::Duration::from_millis(20);

/// Reads the channel's named pipe without ever parking inside `ReadFile`.
///
/// The shepherd hands a Windows app **one** pipe instance, so the reader and
/// the writer are two handles onto one kernel file object: `try_clone` is
/// `DuplicateHandle`, which duplicates the handle and not the object beneath
/// it. That object is opened synchronously, and the I/O manager serialises
/// every operation on a synchronous file object. A `ReadFile` waiting for a
/// message that has no reason to be coming holds the object for as long as
/// it waits, and the writer thread's `WriteFile` queues behind it.
///
/// Nothing breaks that on its own, because the two sides are each waiting
/// for the other: the shepherd will not send anything until it has heard
/// `ready`, and the app cannot send `ready` until the shepherd sends
/// something. An app linking this crate would hang at startup and
/// `wait_ready` would time out with nothing anywhere saying why. Measured on
/// real Windows before this type existed -- the write never returned, and
/// came back only when the pipe was torn down.
///
/// So this half never waits inside the kernel. `PeekNamedPipe` reports what
/// is already buffered and returns either way, and a `ReadFile` is issued
/// only for bytes it has just been told are sitting there. The file object
/// is held for the length of a copy rather than the length of a wait, and
/// the writer gets in between polls.
///
/// Opening the pipe a second time would be the other way out, and it is not
/// available: the shepherd creates a single instance and accepts once, so a
/// second `CreateFile` reaches an instance nothing on the far side will ever
/// read.
#[cfg(windows)]
#[derive(Debug)]
pub(crate) struct PipeReader {
    /// The channel's pipe. Also duplicated for the writer thread, which is
    /// the whole reason this type cannot simply block.
    pipe: std::fs::File,
}

#[cfg(windows)]
impl PipeReader {
    /// How many bytes are buffered, or `None` once the shepherd has closed
    /// its end and everything it sent has been drained.
    ///
    /// # Errors
    ///
    /// Whatever `PeekNamedPipe` reports, except the two codes that mean the
    /// far end is gone. Those are this channel's end of stream, which
    /// `serve`'s reader loop ends on cleanly, and not a failure to pass up.
    fn buffered(&self) -> std::io::Result<Option<u32>> {
        use std::os::windows::io::AsRawHandle as _;

        use windows_sys::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED};
        use windows_sys::Win32::System::Pipes::PeekNamedPipe;

        let mut available: u32 = 0;
        // SAFETY: `self.pipe` is this process's open channel, borrowed for
        // the length of the call, so its handle cannot be closed underneath
        // it. `connect` opened it for reading, which is what `PeekNamedPipe`
        // requires of a handle. A null `lpBuffer` with a zero `nBufferSize`
        // is the documented way to ask for the counts without copying any
        // data out, and the three count pointers this call does not want are
        // null, which the same documentation permits. The one it does want
        // is `&raw mut available`, which points at an initialised `u32` this
        // frame owns and outlives the call.
        #[allow(unsafe_code)]
        let reported = unsafe {
            PeekNamedPipe(
                self.pipe.as_raw_handle(),
                core::ptr::null_mut(),
                0,
                core::ptr::null_mut(),
                &raw mut available,
                core::ptr::null_mut(),
            )
        };
        if reported == 0 {
            let error = std::io::Error::last_os_error();
            let ended = matches!(
                error.raw_os_error(),
                Some(code)
                    if code == ERROR_BROKEN_PIPE as i32
                        || code == ERROR_PIPE_NOT_CONNECTED as i32
            );
            return if ended { Ok(None) } else { Err(error) };
        }
        Ok(Some(available))
    }
}

#[cfg(windows)]
impl std::io::Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let Some(buffered) = self.buffered()? else {
                return Ok(0);
            };
            if buffered == 0 {
                std::thread::sleep(PIPE_POLL_INTERVAL);
                continue;
            }
            // Never more than the peek just reported. Asking for more is
            // what would park this thread in the kernel again and bring
            // back the deadlock this type exists to avoid.
            let want = buf.len().min(buffered as usize);
            return self.pipe.read(&mut buf[..want]);
        }
    }
}

/// Hands the reader thread its half, wrapped in whatever this platform
/// needs it to be.
#[cfg(unix)]
fn read_half(transport: Transport) -> ReadHalf {
    transport
}

/// Hands the reader thread its half, wrapped in whatever this platform
/// needs it to be.
#[cfg(windows)]
fn read_half(transport: Transport) -> ReadHalf {
    PipeReader { pipe: transport }
}

/// Guards the inherited channel from being taken twice in one process.
///
/// Consumed only by the branch that actually takes a channel. A refusal --
/// `Endpoint::Absent`, or a `Descriptor`/`Pipe` naming a mechanism this
/// platform does not have -- takes nothing and must not consume it, or a
/// later, legitimate call would be refused for no reason.
static CHANNEL_TAKEN: AtomicBool = AtomicBool::new(false);

/// Opens the endpoint, returning the reader's half and the writer's clone.
///
/// # Errors
///
/// - [`ChannelError::Unusable`] when the endpoint names a mechanism this
///   platform does not have.
/// - [`ChannelError::Io`] when the pipe cannot be opened or the descriptor
///   cannot be cloned.
/// - [`ChannelError::AlreadyTaken`] when this process already took its
///   channel.
pub(crate) fn connect(endpoint: &Endpoint) -> Result<(ReadHalf, Transport), ChannelError> {
    let transport = match endpoint {
        #[cfg(unix)]
        Endpoint::Descriptor(fd) => {
            if CHANNEL_TAKEN.swap(true, Ordering::SeqCst) {
                return Err(ChannelError::AlreadyTaken);
            }
            use std::os::fd::FromRawFd as _;
            // SAFETY: the shepherd hands this process one descriptor for the
            // channel and names its number in `SHEP_CHANNEL_FD`.
            // `CHANNEL_TAKEN`, swapped just above, makes this arm reachable
            // at most once per process, and nothing else in this crate
            // touches that number, so this call takes sole ownership of it.
            // `discover` is the only path that builds a `Descriptor` from
            // the environment and it refuses anything below 3, so the two
            // cases that are plainly not an owned descriptor -- a negative
            // number, and this process's own standard streams -- never
            // reach here. What is left is a number in range that the
            // environment names wrongly; the standard library's socket
            // calls report that as `EBADF` on first use.
            #[allow(unsafe_code)]
            unsafe {
                Transport::from_raw_fd(*fd)
            }
        }
        #[cfg(windows)]
        Endpoint::Pipe(path) => {
            // Opened BEFORE the claim, which is the opposite order to the
            // unix arm above, because this step can fail and that one
            // cannot. `from_raw_fd` always takes the descriptor, so
            // claiming first is honest there. An `open` that fails after a
            // claim would leave the channel marked taken by a process that
            // never took it, and every later attempt would be refused.
            let opened = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .map_err(ChannelError::Io)?;
            if CHANNEL_TAKEN.swap(true, Ordering::SeqCst) {
                // Defensive rather than expected. The shepherd creates
                // one pipe instance, so a second caller's `open` above
                // should fail busy long before it reaches this swap.
                // Dropping is still the right answer if it ever does:
                // two owners of one pipe is worse than a refusal.
                drop(opened);
                return Err(ChannelError::AlreadyTaken);
            }
            opened
        }
        #[cfg(unix)]
        Endpoint::Pipe(path) => {
            return Err(ChannelError::Unusable(format!(
                "{PIPE_VAR}={} names a Windows named pipe and this is not Windows",
                path.display()
            )));
        }
        #[cfg(windows)]
        Endpoint::Descriptor(fd) => {
            return Err(ChannelError::Unusable(format!(
                "{FD_VAR}={fd} names an inherited descriptor and Windows does not inherit one"
            )));
        }
        Endpoint::Absent => {
            return Err(ChannelError::Unusable(
                "no channel: neither variable is set".to_string(),
            ));
        }
    };
    // Whether a failure here releases the claim depends on what was taken,
    // and the two platforms differ.
    //
    // On unix the descriptor is owned for the life of the process the
    // moment `from_raw_fd` returns, and there is no way to hand a raw
    // descriptor back, so the channel really is taken however this ends.
    //
    // On Windows nothing irrevocable has happened. `transport` is a handle
    // that closes cleanly when it drops, and the pipe can be opened again,
    // so a claim kept here would refuse a later attempt that might well
    // have worked, and would refuse it as `AlreadyTaken` by a process
    // holding nothing.
    let writer = match transport.try_clone() {
        Ok(writer) => writer,
        Err(error) => {
            // Drop the handle before clearing the claim, in that order,
            // so the pipe is actually free at the moment the channel
            // reads as free. Without the explicit drop, `transport`
            // lives until this function returns and the flag goes false
            // while this thread still holds the pipe open.
            //
            // A racing caller fails at its `open`, not at its claim:
            // this arm opens first and claims second, so in that window
            // the other thread never reaches the swap.
            #[cfg(windows)]
            {
                drop(transport);
                CHANNEL_TAKEN.store(false, Ordering::SeqCst);
            }
            return Err(ChannelError::Io(error));
        }
    };
    Ok((read_half(transport), writer))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::fd::IntoRawFd as _;
    #[cfg(unix)]
    use std::os::unix::net::UnixStream;

    use super::*;

    /// fails if [`Endpoint`]'s `Debug` starts redacting, or stops printing
    /// the path at all. Both are reasonable things for someone to do to a
    /// type carrying an environment value, which is exactly why the
    /// decision not to is pinned here rather than left to a derive nobody
    /// revisits. The reasoning is on the type; change it there first if
    /// this test is ever meant to fail.
    ///
    /// Platform-independent despite naming a Windows path: `Path`'s `Debug`
    /// escapes a backslash the same way on every target, so the expected
    /// string below is the same one a unix run produces.
    #[test]
    fn the_pipe_endpoint_prints_its_path_in_full() {
        let endpoint = Endpoint::Pipe(PathBuf::from(
            r"\\.\pipe\shep-channel-1234-0-0123456789abcdef0123456789abcdef",
        ));
        assert_eq!(
            format!("{endpoint:?}"),
            r#"Pipe("\\\\.\\pipe\\shep-channel-1234-0-0123456789abcdef0123456789abcdef")"#
        );
    }

    /// fails if the other two variants start carrying something they did
    /// not, which is the cheap half of the same guard.
    #[test]
    fn the_other_endpoints_print_only_what_they_hold() {
        assert_eq!(format!("{:?}", Endpoint::Descriptor(3)), "Descriptor(3)");
        assert_eq!(format!("{:?}", Endpoint::Absent), "Absent");
    }

    /// fails if a descriptor number the shepherd cannot have passed is
    /// accepted. `from_raw_fd` wants a valid owned descriptor and -1 is
    /// not one, so this is the case the SAFETY comment in `connect` used to
    /// over-claim about.
    #[test]
    fn a_negative_descriptor_is_refused() {
        match descriptor_from("-1") {
            Err(ChannelError::Unusable(what)) => assert!(
                what.contains(&format!("{FD_VAR}=-1")),
                "the refusal names neither the variable nor the value: {what}"
            ),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// fails if `SHEP_CHANNEL_FD=1` is accepted. Worse in practice than a
    /// number that is merely wrong: taking 1 would give this crate
    /// ownership of the app's stdout, write JSON into it, and close it on
    /// drop.
    #[test]
    fn stdout_is_refused_as_a_descriptor() {
        match descriptor_from("1") {
            Err(ChannelError::Unusable(what)) => assert!(
                what.contains(&format!("{FD_VAR}=1")),
                "the refusal names neither the variable nor the value: {what}"
            ),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// fails if the floor is set so high it rejects the descriptor the
    /// shepherd actually passes, which would make the two refusals above
    /// pass for the wrong reason.
    #[test]
    fn the_descriptor_the_shepherd_passes_is_accepted() {
        assert!(matches!(
            descriptor_from(" 3 "),
            Ok(Endpoint::Descriptor(FIRST_INHERITABLE_FD))
        ));
    }

    /// The only test in this crate that calls `connect()`. `CHANNEL_TAKEN`
    /// is process-global and tests share one process, so a second test
    /// that called `connect()` would find the channel already taken by
    /// this one -- there is nowhere else to put that test.
    ///
    /// Asserts the transition, not just the second call's error: the first
    /// call must succeed in this same test, or a `connect()` that refused
    /// everything would pass this test for the wrong reason.
    #[cfg(unix)]
    #[test]
    fn a_descriptor_can_only_be_taken_once() {
        let (ours, _theirs) = UnixStream::pair().expect("socketpair");
        let fd = ours.into_raw_fd();
        let endpoint = Endpoint::Descriptor(fd);

        let first = connect(&endpoint);
        assert!(first.is_ok(), "first take should succeed: {first:?}");

        let second = connect(&endpoint);
        assert!(matches!(second, Err(ChannelError::AlreadyTaken)));

        // `first` stays bound (not `let _ = ...`) through both calls above,
        // so its transport keeps the descriptor open rather than closing it
        // out from under `second`.
        drop(first);
    }
}
