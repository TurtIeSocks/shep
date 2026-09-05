//! Adopting an inherited descriptor: this crate's only `unsafe` on unix
//!
//! Daemonization re-execs the CLI detached, and the child reports
//! `{pid, version}` on an inherited pipe. Building an owning [`File`] from a
//! raw number is the one step std offers no safe path for.
//!
//! [`adopt_fd`] is `unsafe fn` rather than a safe wrapper: its ordering
//! precondition is a caller obligation it cannot check from inside.
//! [`adopt_handover_fd`] is safe because inheritance across `execve`
//! discharges that obligation. [`crate::boot`] calls neither and holds an
//! already-owned [`std::fs::File`].
#![allow(unsafe_code)] // every unsafe site in this crate's unix build is in this file

use core::fmt;

use std::fs::File;
use std::os::unix::io::{FromRawFd, RawFd};

/// Lowest fd number this daemon will ever adopt. 0/1/2 are stdio; adopting
/// one into an owning [`File`] would steal it from whatever already holds it.
pub(crate) const RESERVED_FD_FLOOR: RawFd = 3;

/// Whether `fd` is a number this process could adopt at all: not stdio, and
/// open right now.
///
/// [`adopt_fd`]'s two checks without taking ownership, for the predecessor's
/// pre-exec rehearsal in `handover::adopt::dry_run`. Safe because it owns
/// nothing afterwards, so the recycling hazard has nowhere to land.
///
/// # Errors
/// - [`SysError::ReservedFd`]: `fd` is below 3 (stdio is owned elsewhere).
/// - [`SysError::BadFd`]: `fd` names no open descriptor in this process,
///   which is what a blob naming a descriptor that did not survive the exec
///   looks like.
pub fn adoptable_fd(fd: RawFd) -> Result<(), SysError> {
    if fd < RESERVED_FD_FLOOR {
        return Err(SysError::ReservedFd(fd));
    }
    // `F_GETFD` proves only that the number is open right now, never who
    // opened it; that half of the contract is `adopt_fd`'s caller's.
    nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFD).map_err(|errno| SysError::BadFd {
        fd,
        errno: errno.to_string(),
    })?;
    Ok(())
}

/// Takes ownership of a descriptor inherited across `exec`.
///
/// # Errors
/// - [`SysError::ReservedFd`]: `fd` is below 3 (stdio is owned elsewhere).
/// - [`SysError::BadFd`]: `fd` names no open descriptor in this process.
///
/// # Safety
///
/// Call this before this process opens or closes any descriptor of its own.
/// `F_GETFD` proves only that a number is open now, never what it names, so
/// a later call can adopt a number the kernel recycled into something this
/// process already owns, and the returned [`File`] closes that on drop.
pub unsafe fn adopt_fd(fd: RawFd) -> Result<File, SysError> {
    adoptable_fd(fd)?;
    // SAFETY: `adoptable_fd` returned `Ok`, so `fd` is at or above
    // `RESERVED_FD_FLOOR` and names a descriptor open in this process. The
    // caller's `# Safety` contract is what proves it is the intended
    // inherited pipe; the returned `File` becomes that number's sole owner.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Takes ownership of a descriptor a handover blob names.
///
/// [`adopt_fd`] for one situation: this process is a handover's successor and
/// `fd` was named in the blob at `$SHEP_HOME/run/handover.json` before the
/// `execve`. Safe because an inherited descriptor is open before the new
/// image runs, so no number this process opens later can collide with it; do
/// not call it on a descriptor number from anywhere else.
/// `handover::adopt::adopt` refuses a repeated number.
///
/// # Errors
/// - [`SysError::ReservedFd`]: `fd` is below 3 (stdio is owned elsewhere).
/// - [`SysError::BadFd`]: `fd` names no open descriptor in this process.
pub fn adopt_handover_fd(fd: RawFd) -> Result<File, SysError> {
    // SAFETY: `fd` was inherited across `execve` from this process's own
    // predecessor, which cleared `FD_CLOEXEC` on it and named it in the
    // handover blob. It was open before this image ran, so no number this
    // process opens can share it.
    unsafe { adopt_fd(fd) }
}

/// Errors adopting an inherited descriptor.
///
/// `#[non_exhaustive]`: a future adoption check would need its own variant
/// rather than stretching [`Self::BadFd`] past what `fcntl` reported.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SysError {
    /// The descriptor number cannot be adopted: negative, or below 3
    /// (stdio is owned elsewhere).
    ReservedFd(RawFd),
    /// The descriptor is not open in this process (carries the errno name).
    BadFd {
        /// The descriptor number that was rejected.
        fd: RawFd,
        /// The OS error name/message `fcntl` reported.
        errno: String,
    },
}

impl fmt::Display for SysError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReservedFd(fd) if *fd < 0 => {
                write!(f, "fd {fd} is negative and cannot name a descriptor")
            }
            Self::ReservedFd(fd) => {
                write!(f, "fd {fd} is reserved for stdio and cannot be adopted")
            }
            Self::BadFd { fd, errno } => {
                write!(
                    f,
                    "fd {fd} is not an open descriptor in this process: {errno}"
                )
            }
        }
    }
}

impl core::error::Error for SysError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::io::{AsRawFd, IntoRawFd};

    #[test]
    fn a_real_inherited_descriptor_is_adopted_and_owned() {
        // `into_raw_fd` leaves the descriptor in the state an exec-inherited
        // one is in: live, and owned by nobody.
        let (parent, child) = std::os::unix::net::UnixStream::pair().unwrap();
        let fd = child.into_raw_fd();
        {
            // SAFETY: this test process has opened nothing else of its own
            // between creating the pair and adopting it.
            let mut adopted = unsafe { adopt_fd(fd) }.unwrap();
            std::io::Write::write_all(&mut adopted, b"hello\n").unwrap();
        } // dropping the File closes the descriptor
        let mut read = String::new();
        let mut parent = parent;
        parent.read_to_string(&mut read).unwrap();
        assert_eq!(
            read, "hello\n",
            "EOF proves the adopted descriptor was closed on drop"
        );
    }

    #[test]
    fn stdio_numbers_are_refused() {
        for fd in 0..3 {
            // SAFETY: adopt_fd refuses fd < 3 before touching anything, so
            // there is no precondition to uphold for a call that never adopts.
            let result = unsafe { adopt_fd(fd) };
            assert_eq!(result.unwrap_err(), SysError::ReservedFd(fd));
        }
    }

    #[test]
    fn a_negative_fd_is_refused_with_an_accurate_message() {
        // SAFETY: `adopt_fd` refuses fd < 3 before touching anything.
        let err = unsafe { adopt_fd(-1) }.unwrap_err();
        assert_eq!(err, SysError::ReservedFd(-1));
        assert!(
            err.to_string().contains("negative"),
            "a negative fd is not \"reserved for stdio\": {err}"
        );
    }

    #[test]
    fn a_closed_descriptor_is_refused_instead_of_adopted() {
        // A high number, not the pair's own: unix hands out the lowest free
        // fd, so once this one is closed no concurrent test in this binary is
        // handed it back ahead of the ~2048 lower free ones. That floor is
        // what makes the second adoption a real `BadFd` probe, not a race.
        const PROBE_FD: RawFd = 2048;
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        let parked = nix::fcntl::fcntl(a.as_raw_fd(), nix::fcntl::FcntlArg::F_DUPFD(PROBE_FD))
            .expect("duplicating onto a high fd number must succeed");
        assert!(
            parked >= PROBE_FD,
            "F_DUPFD must return a number at or above its floor, got {parked}"
        );
        // SAFETY: `parked` is a descriptor this test just created and owns
        // outright; `a` keeps its own separate descriptor and its own Drop.
        drop(unsafe { adopt_fd(parked) }.unwrap()); // adopt once, closing it
        // SAFETY: `parked` is closed now, and being >= PROBE_FD it cannot be
        // reallocated while lower numbers remain free, so this second
        // attempt probes a genuinely closed descriptor. BadFd is expected to
        // refuse it before `from_raw_fd` ever runs.
        let second = unsafe { adopt_fd(parked) };
        assert!(matches!(second, Err(SysError::BadFd { .. })));
    }

    #[test]
    fn a_fd_this_process_never_owned_is_refused() {
        // fd 4096 is a number this process never owns: fd-table limits sit
        // far below it, so `F_GETFD` fails deterministically even under a
        // parallel harness.
        // SAFETY: fd 4096 is never open in this process; adopt_fd's
        // F_GETFD probe rejects it before from_raw_fd ever runs, so there
        // is no ordering precondition to uphold for a call that never
        // actually adopts.
        let err = unsafe { adopt_fd(4096) }.unwrap_err();
        assert!(
            matches!(err, SysError::BadFd { fd: 4096, .. }),
            "a fd this process never owned must be refused as BadFd: {err:?}"
        );
    }
}
