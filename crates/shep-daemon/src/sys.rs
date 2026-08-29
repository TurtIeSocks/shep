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
//!     since exec — this is a REAL hazard, not a theoretical one:
//!     `F_GETFD` only proves a number is open RIGHT NOW, never who opened
//!     it, so it cannot distinguish a genuinely inherited descriptor from a
//!     number the daemon's OWN later steps happened to reuse. Concretely:
//!     if adoption ran *after* `bind_socket`/`write_pidfile` had already
//!     opened (and, for the pidfile's temp file, closed) descriptors of
//!     their own, a stale `SHEP_READY_FD` could land on the freshly-bound
//!     listener's own fd, and dropping the wrongly-adopted `File` would
//!     close that listener out from under `tokio`. This is exactly why
//!     [`adopt_fd`] is `unsafe fn`: the precondition that actually closes
//!     this hole — call it before this process has opened any descriptor
//!     of its own — is a CALLER obligation no amount of internal checking
//!     can verify from inside `adopt_fd` itself, so the type system forces
//!     every call site to write down its own justification instead of
//!     letting the invariant erode silently on a future reorder. `boot`
//!     never calls [`adopt_fd`] at all, so it is not possible for anything
//!     `boot` itself does — bind a socket, open a tempfile, install signal
//!     handlers — to land between adoption and use. The intended
//!     PRODUCTION caller is the CLI's `main` (Phase 3), which discharges
//!     the ordering precondition by being the literal first fd-touching
//!     statement of the whole process, before a tokio runtime — the thing
//!     that would make `boot`'s own attempt at this structurally
//!     impossible to guarantee, since `boot` is `async` and a runtime with
//!     its own live poller fds necessarily exists before `boot` is ever
//!     called — even exists. See [`crate::boot::BootOptions::ready_fd`]'s
//!     own doc;
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
    // that constructs a `File` from a bare fd. `boot` never calls it — see
    // this fn's own doc above — and this crate has no other production
    // caller today either; every in-crate call site is one of this file's
    // own tests, each adopting a fd it just created and each doing so at
    // most once per descriptor. The `File` returned here becomes that
    // number's sole owner; nothing else will read, write, or close it
    // again.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Errors adopting an inherited descriptor.
///
/// `#[non_exhaustive]`: today's two variants cover a disqualified fd number
/// and one the OS says is not open, and a future adoption check — rejecting
/// an fd that is open but of the wrong kind, not a socket where one is
/// required — would need its own variant rather than stretching
/// [`Self::BadFd`] past what `fcntl` actually told the caller, and
/// shep-daemon is a published library an out-of-tree matcher should not
/// break for (IR-20).
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
        // No fd-reuse hazard here, and so no lock: this adopts a descriptor
        // it just created and still holds open. Nothing is closed and then
        // re-probed by number, which is the only shape that can race another
        // test over fd reuse (see
        // `a_closed_descriptor_is_refused_instead_of_adopted`).
        //
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
        // Park the probe on a HIGH descriptor number instead of whatever the
        // kernel handed the pair. Unix allocates the LOWEST free fd, so once
        // this one is closed below, no other concurrently-running test in
        // this binary can be handed the number back ahead of the ~2048 lower
        // free ones — which is what makes the second adoption a genuine
        // BadFd probe rather than a race.
        //
        // Re-probing the pair's own LOW fd under a lock cannot work: the
        // lock only excludes tests that take it, while every OTHER test in
        // the binary remains free to open a file and be handed the
        // just-closed number. `adopt_fd`'s `F_GETFD` probe would then
        // succeed (the number IS open — it belongs to someone else now),
        // the adoption would go through, and dropping the returned `File`
        // would double-close another test's descriptor. The high number
        // keeps this closed only as long as this process has fewer than
        // ~2048 descriptors open at once; it is not a structural
        // guarantee, just a floor no test in this suite comes close to. If
        // parallel tests ever pushed descriptor use past it, `parked`
        // could be reused before the second `adopt_fd` below runs. No lock
        // is needed for the concurrency this suite actually reaches, not
        // because the race is impossible at any concurrency.
        const PROBE_FD: RawFd = 2048;
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        let parked = nix::fcntl::fcntl(a.as_raw_fd(), nix::fcntl::FcntlArg::F_DUPFD(PROBE_FD))
            .expect("duplicating onto a high fd number must succeed");
        assert!(
            parked >= PROBE_FD,
            "F_DUPFD must return a number at or above its floor, got {parked}"
        );
        // SAFETY: `parked` is a descriptor this test just created and owns
        // outright — `a` keeps its own separate descriptor and its own Drop.
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
        // `BootOptions::ready_fd` is `Option<std::fs::File>`, so there is no
        // way to drive a bad fd NUMBER through `boot`'s public API at all —
        // the type itself proves the handle was valid at construction. The
        // BadFd-refusal behavior this test pins belongs here, testing
        // `adopt_fd` directly, which is where the refusal actually happens.
        //
        // fd 4096 is a number this process will NEVER own: default
        // fd-table limits sit far below it, and nothing in this crate's
        // test suite opens anywhere near that many concurrent descriptors,
        // so `F_GETFD` fails on it deterministically, every time, regardless
        // of what else is running concurrently — zero collision risk. Same
        // reasoning as the high `PROBE_FD` in
        // `a_closed_descriptor_is_refused_instead_of_adopted` above: staying
        // clear of the numbers the kernel actually hands out is what makes
        // an fd-refusal test deterministic under a parallel harness.
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
