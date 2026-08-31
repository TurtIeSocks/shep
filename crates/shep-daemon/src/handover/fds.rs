//! Clearing `FD_CLOEXEC` on a descriptor the handover carries, and reading
//! it back.
//!
//! Every descriptor in the handover blob passes through [`keep_across_exec`]
//! exactly once, deliberately, before the daemon `execv`s its successor.
//! Everything the daemon opens is close-on-exec by default (verified
//! against pinned `mio`, `tokio` and `std` sources), so a descriptor this
//! module never touches is closed by the kernel at the exec boundary and
//! silently dropped from the new image. For a sheep's stdout read end, that
//! does not lose the sheep's output: the child blocks on `write()` once the
//! 64KiB pipe buffer fills, and hangs, which reads as an application bug
//! not a shep one.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};

use nix::fcntl::{FcntlArg, FdFlag, fcntl};

/// Clear `FD_CLOEXEC` on `fd`, so it survives this process's next
/// `execve`.
///
/// Reads the descriptor's current flags with `F_GETFD`, clears only the
/// `FD_CLOEXEC` bit, and writes the result back with `F_SETFD`. A bare
/// `F_SETFD(FdFlag::empty())` would clobber any other flag the descriptor
/// carries, and would not be idempotent: a second call would re-derive
/// `empty()` from whatever the first call left rather than from the
/// descriptor's real flags, so the operation would happen to look
/// idempotent only because it always writes the same clobbered value.
///
/// Do not call this on a descriptor the successor will not adopt: every fd
/// left `FD_CLOEXEC`-clear leaks into the new process image across the
/// exec, whether or not the handover blob names it.
///
/// # Errors
///
/// Returns an error if `fcntl` fails, which on a valid open descriptor
/// should not happen in practice.
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
/// The handover blob names descriptors as numbers, not as borrows, and the
/// borrowed form is reached only through `BorrowedFd::borrow_raw`, which is
/// unsafe and would have to live in `sys.rs` (IR-22/23). `fcntl` on a
/// number needs neither, so the number is the primitive here and the
/// borrowed form delegates to it.
///
/// The same warning applies: call this only for a descriptor the successor
/// will adopt.
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
/// The inverse of that function, bit for bit: reads the current flags,
/// sets only `FD_CLOEXEC`, writes them back. Nothing else the descriptor
/// carries is disturbed.
///
/// Restoring is the right direction and not merely the symmetric one.
/// Everything the daemon opens is close-on-exec at creation, per this
/// module's own preamble, so the flag this puts back is the flag the
/// descriptor had before the handover cleared it.
///
/// # Errors
///
/// Returns an error if `fcntl` fails, which on a valid open descriptor
/// should not happen in practice.
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
/// The duplicate refers to the same open file description, so every question
/// an adoption asks answers identically through it: what kind of object it
/// is, which direction it is open for, whether a socket is connected, which
/// status flags it carries. What it does not share is ownership, and that is
/// the whole point — closing the duplicate leaves the original open and
/// still owned by whoever in this process owns it.
///
/// `F_DUPFD_CLOEXEC` rather than a bare `dup(2)`, and both halves of that
/// are load-bearing here. The floor keeps a duplicate off stdio, so
/// [`crate::sys::adoptable_fd`] cannot refuse it as reserved and report a
/// problem the blob does not have. Close-on-exec means a duplicate that
/// somehow outlived its inspection still cannot leak into a successor's
/// image, which is precisely the failure [`close_raw_after_exec`] exists to
/// undo for the named descriptors.
///
/// # Errors
///
/// Returns an error if `fcntl` fails, which is what a number naming no open
/// descriptor looks like, and what a process at its descriptor limit looks
/// like too.
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
/// Returns an error if `fcntl` fails, which on a valid open descriptor
/// should not happen in practice.
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
        // Establishes the default this whole phase depends on. If this ever
        // fails, std changed and the fd inventory needs re-auditing.
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
