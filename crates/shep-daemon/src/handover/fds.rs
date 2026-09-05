//! Clearing `FD_CLOEXEC` on a descriptor the handover carries, and reading
//! it back.
//!
//! Everything the daemon opens is close-on-exec, so a descriptor this module
//! never touches is closed by the kernel at the exec boundary and dropped
//! from the successor's image.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};

use nix::fcntl::{FcntlArg, FdFlag, fcntl};

/// Clear `FD_CLOEXEC` on `fd`, so it survives this process's next
/// `execve`.
///
/// Only that bit is cleared, so any other flag the descriptor carries
/// survives and a second call is a no-op. Call it only for a descriptor the
/// successor will adopt: whatever is left clear leaks into the new image,
/// blob or no blob.
///
/// # Errors
///
/// Returns an error if `fcntl` fails.
#[allow(
    dead_code,
    reason = "exercised by this module's own tests; production goes through \
                             `keep_raw_across_exec`, which a blob's numbers are all it has"
)]
pub fn keep_across_exec(fd: BorrowedFd<'_>) -> io::Result<()> {
    keep_raw_across_exec(fd.as_raw_fd())
}

/// [`keep_across_exec`], for a descriptor known only by its number.
///
/// The blob names descriptors as numbers, so this is the primitive and the
/// borrowed form delegates to it. Call it only for a descriptor the
/// successor will adopt.
///
/// # Errors
///
/// Returns an error if `fcntl` fails, which is what a number naming no open
/// descriptor looks like.
pub fn keep_raw_across_exec(fd: RawFd) -> io::Result<()> {
    let flags = fcntl(fd, FcntlArg::F_GETFD)?;
    let mut flags = FdFlag::from_bits_truncate(flags);
    flags.remove(FdFlag::FD_CLOEXEC);
    fcntl(fd, FcntlArg::F_SETFD(flags))?;
    Ok(())
}

/// Set `FD_CLOEXEC` on `fd` again, undoing a [`keep_raw_across_exec`] whose
/// exec never happened.
///
/// Only that bit is touched. Everything the daemon opens is close-on-exec at
/// creation, so this restores the flag the descriptor had.
///
/// # Errors
///
/// Returns an error if `fcntl` fails.
pub fn close_raw_after_exec(fd: RawFd) -> io::Result<()> {
    let flags = fcntl(fd, FcntlArg::F_GETFD)?;
    let mut flags = FdFlag::from_bits_truncate(flags);
    flags.insert(FdFlag::FD_CLOEXEC);
    fcntl(fd, FcntlArg::F_SETFD(flags))?;
    Ok(())
}

/// Duplicate `fd` onto a fresh number, for a caller that needs to inspect a
/// descriptor it must not take.
///
/// The duplicate shares the open file description but not ownership, so
/// closing it leaves the original open. `F_DUPFD_CLOEXEC` above
/// [`crate::sys::RESERVED_FD_FLOOR`], so [`crate::sys::adoptable_fd`] cannot
/// refuse it as reserved and it cannot leak into a successor's image.
///
/// # Errors
///
/// Returns an error if `fcntl` fails: no such descriptor, or a process at
/// its descriptor limit.
pub fn duplicate_raw(fd: RawFd) -> io::Result<RawFd> {
    Ok(fcntl(
        fd,
        FcntlArg::F_DUPFD_CLOEXEC(crate::sys::RESERVED_FD_FLOOR),
    )?)
}

/// Whether `fd` currently survives an `execve` (i.e. `FD_CLOEXEC` is
/// clear).
///
/// # Errors
///
/// Returns an error if `fcntl` fails.
#[allow(dead_code, reason = "read by this module's own tests")]
pub fn is_kept(fd: BorrowedFd<'_>) -> io::Result<bool> {
    let flags = fcntl(fd.as_raw_fd(), FcntlArg::F_GETFD)?;
    let flags = FdFlag::from_bits_truncate(flags);
    Ok(!flags.contains(FdFlag::FD_CLOEXEC))
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;

    #[test]
    fn a_fresh_pipe_is_close_on_exec_and_can_be_kept() {
        let (r, _w) = std::io::pipe().unwrap();
        assert!(
            !super::is_kept(r.as_fd()).unwrap(),
            "std pipes are CLOEXEC by default"
        );
        super::keep_across_exec(r.as_fd()).unwrap();
        assert!(super::is_kept(r.as_fd()).unwrap());
    }

    #[test]
    fn keeping_is_idempotent() {
        let (r, _w) = std::io::pipe().unwrap();
        super::keep_across_exec(r.as_fd()).unwrap();
        super::keep_across_exec(r.as_fd()).unwrap();
        assert!(super::is_kept(r.as_fd()).unwrap());
    }
}
