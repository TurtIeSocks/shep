//! Adopting an inherited descriptor — this crate's only `unsafe`
//!
//! **Why unsafe here, and nowhere else.** The daemonization contract (spec
//! §3) is that the CLI re-execs itself detached and the child reports
//! `{pid, version}` on an inherited pipe once the socket is bound. Adopting
//! an inherited descriptor is the one operation std offers no safe path
//! for: `OwnedFd`/`File` can only be built from a raw number through
//! `from_raw_fd`, which is unsafe because nothing in the type system proves
//! the number names a descriptor this process owns.
//!
//! **All of it, confined to this file (IR-22).** [`adopt_fd`] is `unsafe
//! fn`, not a safe fn hiding an internal unsafe block: the ordering
//! precondition that makes adoption sound (call this before the process
//! opens anything of its own — see scenario (c) below) is a CALLER
//! obligation this function cannot verify from inside itself, so the type
//! system pushes it out to whoever calls it. [`crate::boot::BootOptions::ready_fd`]
//! is `Option<`[`std::fs::File`]`>`, an already-owned handle — `boot`
//! itself never calls this function and contains no unsafe of its own (see
//! that field's own doc). The crate's actual unsafe surface today, counted
//! honestly: [`adopt_fd`]'s own definition here, plus this file's own
//! test-only call sites exercising it directly against synthetic fds (see
//! below) — every syntactic site lives in this one file. The intended
//! PRODUCTION caller is the CLI's `main` (Phase 3), a different crate, as
//! its literal first fd-touching statement, before a tokio runtime even
//! exists — not written yet, so it adds no site here today. What actually
//! matters for soundness is unaffected by exactly how many test sites this
//! file accumulates: only a real production call's ordering claim has to
//! hold against a genuinely inherited descriptor; every test site adopts a
//! fd it created itself moments earlier in the same test, which is a
//! narrower, locally-checkable obligation, not a widening of the
//! exception.
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
//!     since exec — this is a REAL hazard, not a theoretical one, and an
//!     earlier version of this essay was wrong to call it "impossible in
//!     practice": `F_GETFD` only proves a number is open RIGHT NOW, never
//!     who opened it, so it cannot distinguish a genuinely inherited
//!     descriptor from a number the daemon's OWN later steps happened to
//!     reuse. Concretely: an earlier version of `boot()` called adoption
//!     *after* `bind_socket`/`write_pidfile` had already opened (and, for
//!     the pidfile's temp file, closed) descriptors of their own — a stale
//!     `SHEP_READY_FD` could then land on the freshly-bound listener's own
//!     fd, and dropping the wrongly-adopted `File` closed that listener out
//!     from under `tokio`, reproducibly, through nothing more exotic than
//!     the ordinary `boot()` API (`BootOptions { ready_fd: Some(stale) }`).
//!     This is exactly why [`adopt_fd`] is `unsafe fn`: the precondition
//!     that actually closes this hole — call it before this process has
//!     opened any descriptor of its own — is a CALLER obligation no amount
//!     of internal checking can verify from inside `adopt_fd` itself, so
//!     the type system now forces every call site to write down its own
//!     justification instead of letting the invariant erode silently on a
//!     future reorder. **Decision 1 (2026-08-08) removed the whole class
//!     of risk this scenario describes from this crate structurally**,
//!     rather than merely re-ordering around it again: `boot` no longer
//!     calls [`adopt_fd`] at all, so it is no longer possible for anything
//!     `boot` itself does — bind a socket, open a tempfile, install signal
//!     handlers — to land between adoption and use. The intended
//!     PRODUCTION caller is the CLI's `main` (Phase 3), which discharges
//!     the ordering precondition by being the literal first fd-touching
//!     statement of the whole process, before a tokio runtime — the thing
//!     that made `boot`'s own attempt at this structurally impossible to
//!     guarantee, since `boot` is `async` and a runtime with its own live
//!     poller fds necessarily exists before `boot` is ever called — even
//!     exists. See [`crate::boot::BootOptions::ready_fd`]'s own doc;
//!
//! (d) double adoption — a future production caller (the CLI's `main`,
//!     Phase 3) is expected to call [`adopt_fd`] at most once, consuming
//!     the number into an owning [`std::fs::File`] that closes it on drop,
//!     so a second production adoption of the same fd should not occur; if
//!     it somehow did, scenario (b)'s `BadFd` refusal catches it, since the
//!     first adoption already closed the number. (This crate's *tests*
//!     call `adopt_fd` directly, and more than once across the suite, each
//!     time against a fd the test itself just created — a different,
//!     lower-stakes situation than production double-adoption, and not
//!     what this scenario is about.)
#![allow(unsafe_code)] // IR-24 exception — seven sites total, all in this file (its own definition plus 6 test call sites; `boot.rs` has none — see the essay above and `crate::boot::BootOptions::ready_fd`'s doc).

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
/// # Safety
///
/// The caller must call this before this process opens (or closes) any
/// descriptor of its own — before binding a socket, opening a file,
/// spawning anything that inherits fds. `F_GETFD` (used internally) only
/// proves a number is CURRENTLY open, never who opened it or what it now
/// names, so calling this after the process has touched its own descriptors
/// risks adopting a number the kernel has since recycled into something the
/// process already owns; dropping the returned [`File`] would then close
/// that resource out from under its real owner instead of the intended
/// inherited pipe. See this module's rationale essay, scenario (c), for a
/// worked example of exactly this happening.
///
/// The intended caller is the CLI's `main` (Phase 3, a different crate —
/// not written yet): its literal first fd-touching statement, before a
/// tokio runtime (or anything else) exists. Nothing in `shep-daemon` calls
/// this function in production today; [`crate::boot::boot`] receives an
/// already-adopted [`std::fs::File`] via
/// [`crate::boot::BootOptions::ready_fd`] and never touches a raw fd
/// itself — see that field's own doc for why adoption moved out of `boot`.
pub unsafe fn adopt_fd(fd: RawFd) -> Result<File, SysError> {
    if fd < RESERVED_FD_FLOOR {
        return Err(SysError::ReservedFd(fd));
    }
    // Probe before adopting: F_GETFD only succeeds on a descriptor this
    // process actually has open right now, so a stale or never-opened
    // number is rejected here instead of being handed to `from_raw_fd`.
    // (This does NOT prove the number is the caller's intended inherited
    // pipe rather than something recycled — that half of the contract is
    // the caller's, per this fn's own `# Safety` section above.)
    nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFD).map_err(|errno| SysError::BadFd {
        fd,
        errno: errno.to_string(),
    })?;
    // SAFETY: `fd` is >= RESERVED_FD_FLOOR (checked above), so it cannot
    // alias stdio, and the `fcntl` probe just above proved it names a
    // descriptor genuinely open in this process. The caller's own contract
    // (this fn's `# Safety` section) is what proves that open descriptor is
    // the intended inherited pipe rather than something this process opened
    // itself in the meantime. `adopt_fd` is the only place in this crate
    // that constructs a `File` from a bare fd, and the daemon's boot path
    // (its one caller) invokes it at most once per descriptor — so the
    // `File` returned here becomes the number's sole owner; nothing else
    // will read, write, or close it again.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Errors adopting an inherited descriptor.
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
    use std::os::unix::io::IntoRawFd;

    // FD_REUSE_LOCK (crate::testing, IR-33's one shared fixture module):
    // closing a real fd and then acting again on that SAME learned number
    // races other concurrently-running tests over fd reuse — see that
    // static's own doc for the real SIGABRT this crashed with before both
    // tests below took the lock.
    use crate::testing::FD_REUSE_LOCK;

    #[test]
    fn a_real_inherited_descriptor_is_adopted_and_owned() {
        let _guard = FD_REUSE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // into_raw_fd gives up std's ownership, which is exactly the state
        // an exec-inherited descriptor is in: live, and owned by nobody yet.
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
            // SAFETY: adopt_fd refuses fd < 3 before touching anything —
            // no precondition to uphold for a call that never adopts.
            let result = unsafe { adopt_fd(fd) };
            assert_eq!(result.unwrap_err(), SysError::ReservedFd(fd));
        }
    }

    #[test]
    fn a_negative_fd_is_refused_with_an_accurate_message() {
        // SAFETY: same as above — refused before any adoption is attempted.
        let err = unsafe { adopt_fd(-1) }.unwrap_err();
        assert_eq!(err, SysError::ReservedFd(-1));
        assert!(
            err.to_string().contains("negative"),
            "a negative fd is not \"reserved for stdio\": {err}"
        );
    }

    #[test]
    fn a_closed_descriptor_is_refused_instead_of_adopted() {
        let _guard = FD_REUSE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        let fd = a.into_raw_fd();
        // SAFETY: this test process has opened nothing else of its own
        // between creating the pair and this first adoption.
        drop(unsafe { adopt_fd(fd) }.unwrap()); // adopt once, closing it
        // SAFETY: fd is now closed; a second adoption attempt is exactly
        // what this test means to probe, and BadFd is expected to refuse
        // it before from_raw_fd ever runs.
        let second = unsafe { adopt_fd(fd) };
        assert!(matches!(second, Err(SysError::BadFd { .. })));
    }

    #[test]
    fn a_fd_this_process_never_owned_is_refused() {
        // Moved here from `boot.rs` by Decision 1 (2026-08-08):
        // `BootOptions::ready_fd` is `Option<std::fs::File>` now, so there
        // is no longer any way to drive a bad fd NUMBER through `boot`'s
        // public API at all — the type itself proves the handle was valid
        // at construction. The BadFd-refusal behavior this test pins used
        // to be exercised indirectly through `boot`; it belongs here now,
        // testing `adopt_fd` directly, which is where the refusal actually
        // happens.
        //
        // fd 4096 is a number this process will NEVER own: default
        // fd-table limits sit far below it, and nothing in this crate's
        // test suite opens anywhere near that many concurrent descriptors,
        // so `F_GETFD` fails on it deterministically, every time, regardless
        // of what else is running concurrently — zero collision risk,
        // unlike `a_closed_descriptor_is_refused_instead_of_adopted` above,
        // which frees and re-probes a REAL fd number and so needs
        // `FD_REUSE_LOCK`. This test needs no lock for exactly that reason.
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
