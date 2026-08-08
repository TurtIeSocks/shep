//! Adopting an inherited descriptor — the phase's only `unsafe`
//!
//! **Why unsafe here, and nowhere else.** The daemonization contract (spec
//! §3) is that the CLI re-execs itself detached and the child reports
//! `{pid, version}` on an inherited pipe once the socket is bound. Adopting
//! an inherited descriptor is the one operation std offers no safe path
//! for: `OwnedFd`/`File` can only be built from a raw number through
//! `from_raw_fd`, which is unsafe because nothing in the type system proves
//! the number names a descriptor this process owns.
//!
//! **Rejected alternative:** have the parent pass a socket path
//! (`SHEP_READY_SOCK`) and let the child connect and write. It is entirely
//! safe and was the first design. Its cost is a second socket in the boot
//! path — one more thing to place inside 0700, unlink, and recover when
//! stale — to replace a five-line adoption, and it puts the readiness
//! handshake on a different mechanism from the one the spec, systemd
//! `Type=notify` integration, and every comparable supervisor use. Not
//! worth it.
//!
//! **Invariant:** the descriptor was inherited across `exec` from our own
//! parent and is not otherwise owned in this process. **Checked, not
//! assumed:** [`adopt_fd`] refuses anything below fd 3 (stdio is owned
//! elsewhere) and calls `fcntl(fd, F_GETFD)` first, so a closed or
//! never-opened number returns [`SysError::BadFd`] instead of being
//! adopted. **Failure scenarios considered:**
//! (a) a hostile `SHEP_READY_FD=1` — refused by the fd-3 floor, so stdout
//!     is never closed underneath the logger;
//! (b) a stale number for a descriptor closed since exec — refused by the
//!     `fcntl` probe;
//! (c) a number that has been *recycled* into another live descriptor
//!     since exec — impossible in practice because the adoption happens
//!     during boot, before the daemon opens anything else, and the number
//!     comes from our own parent process via [`crate::boot::READY_FD_ENV`],
//!     not from a user;
//! (d) double adoption — [`adopt_fd`] is called at most once, from
//!     [`crate::boot::boot`], and consumes the number into an owning
//!     [`std::fs::File`] that closes it on drop.
#![allow(unsafe_code)] // IR-24: the one exception in this crate — see the essay above.

use core::fmt;

use std::fs::File;
use std::os::unix::io::{FromRawFd, RawFd};

/// Lowest fd number this daemon will ever adopt. 0/1/2 are stdio, owned by
/// the logger and inherited-terminal plumbing elsewhere in the process —
/// adopting one of those into an owning [`File`] would silently steal it out
/// from under whatever already holds it.
const RESERVED_FD_FLOOR: RawFd = 3;

/// Takes ownership of a descriptor inherited across `exec`.
///
/// # Errors
/// - [`SysError::ReservedFd`] — `fd` is below 3 (stdio is owned elsewhere).
/// - [`SysError::BadFd`] — `fd` names no open descriptor in this process.
///
/// # Safety-relevant behavior
/// See the module rationale: the descriptor is validated before adoption
/// and the returned [`File`] owns it from then on.
pub fn adopt_fd(fd: RawFd) -> Result<File, SysError> {
    if fd < RESERVED_FD_FLOOR {
        return Err(SysError::ReservedFd(fd));
    }
    // Probe before adopting: F_GETFD only succeeds on a descriptor this
    // process actually has open right now, so a stale or never-opened
    // number is rejected here instead of being handed to `from_raw_fd`.
    nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFD).map_err(|errno| SysError::BadFd {
        fd,
        errno: errno.to_string(),
    })?;
    // SAFETY: `fd` is >= RESERVED_FD_FLOOR (checked above), so it cannot
    // alias stdio, and the `fcntl` probe just above proved it names a
    // descriptor genuinely open in this process. `adopt_fd` is the only
    // place in this crate that constructs a `File` from a bare fd, and the
    // daemon's boot path (its one caller) invokes it at most once per
    // descriptor — so the `File` returned here becomes the number's sole
    // owner; nothing else will read, write, or close it again.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Errors adopting or writing to an inherited descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SysError {
    /// The descriptor number is below 3 and cannot be adopted.
    ReservedFd(RawFd),
    /// The descriptor is not open in this process (carries the errno name).
    BadFd {
        /// The descriptor number that was rejected.
        fd: RawFd,
        /// The OS error name/message `fcntl` reported.
        errno: String,
    },
    /// Writing the readiness line failed (carries the OS message).
    ReadyWrite(String),
}

impl fmt::Display for SysError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReservedFd(fd) => {
                write!(f, "fd {fd} is reserved for stdio and cannot be adopted")
            }
            Self::BadFd { fd, errno } => {
                write!(
                    f,
                    "fd {fd} is not an open descriptor in this process: {errno}"
                )
            }
            Self::ReadyWrite(msg) => write!(f, "writing the readiness line failed: {msg}"),
        }
    }
}

impl core::error::Error for SysError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::io::IntoRawFd;

    #[test]
    fn a_real_inherited_descriptor_is_adopted_and_owned() {
        // into_raw_fd gives up std's ownership, which is exactly the state
        // an exec-inherited descriptor is in: live, and owned by nobody yet.
        let (parent, child) = std::os::unix::net::UnixStream::pair().unwrap();
        let fd = child.into_raw_fd();
        {
            let mut adopted = adopt_fd(fd).unwrap();
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
            assert_eq!(adopt_fd(fd).unwrap_err(), SysError::ReservedFd(fd));
        }
    }

    #[test]
    fn a_closed_descriptor_is_refused_instead_of_adopted() {
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        let fd = a.into_raw_fd();
        drop(adopt_fd(fd).unwrap()); // adopt once, closing it
        assert!(matches!(adopt_fd(fd), Err(SysError::BadFd { .. })));
    }
}
