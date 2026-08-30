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
//! 64KiB pipe buffer fills, and hangs — which reads as an application bug,
//! not a shep one.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};

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
pub fn keep_across_exec(fd: BorrowedFd<'_>) -> io::Result<()> {
    let fd = fd.as_raw_fd();
    let flags = fcntl(fd, FcntlArg::F_GETFD)?;
    let mut flags = FdFlag::from_bits_truncate(flags);
    flags.remove(FdFlag::FD_CLOEXEC);
    fcntl(fd, FcntlArg::F_SETFD(flags))?;
    Ok(())
}

/// Whether `fd` currently survives an `execve` (i.e. `FD_CLOEXEC` is
/// clear).
///
/// # Errors
///
/// Returns an error if `fcntl` fails, which on a valid open descriptor
/// should not happen in practice.
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
