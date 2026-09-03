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

/// Guards the inherited channel from being taken twice in one process.
///
/// Consumed only by the branch that actually takes a channel. A refusal --
/// `Endpoint::Absent`, or a `Descriptor`/`Pipe` naming a mechanism this
/// platform does not have -- takes nothing and must not consume it, or a
/// later, legitimate call would be refused for no reason.
static CHANNEL_TAKEN: AtomicBool = AtomicBool::new(false);

/// Opens the endpoint, returning the transport and a clone for the writer.
///
/// # Errors
///
/// - [`ChannelError::Unusable`] when the endpoint names a mechanism this
///   platform does not have.
/// - [`ChannelError::Io`] when the pipe cannot be opened or the descriptor
///   cannot be cloned.
/// - [`ChannelError::AlreadyTaken`] when this process already took its
///   channel.
pub(crate) fn connect(endpoint: &Endpoint) -> Result<(Transport, Transport), ChannelError> {
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
                // Another caller claimed it while this one was opening.
                // Drop this handle rather than return a second owner.
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
    // `CHANNEL_TAKEN` is not released if this fails: once `transport` above
    // has taken the descriptor, the channel is taken for the life of the
    // process no matter what `try_clone` does next. There is no way to
    // hand a raw descriptor back once `from_raw_fd` owns it.
    let writer = transport.try_clone().map_err(ChannelError::Io)?;
    Ok((transport, writer))
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::fd::IntoRawFd as _;
    #[cfg(unix)]
    use std::os::unix::net::UnixStream;

    use super::*;

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
