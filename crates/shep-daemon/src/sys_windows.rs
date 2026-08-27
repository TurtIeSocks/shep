//! The Windows counterpart to `sys` (the unix-only module of the same
//! role): a job object per sheep
//!
//! This module exists for one reason — a Win32 job object is what replaces
//! the unix process group — and it is this crate's ONLY unsafe surface on
//! Windows, exactly as `sys.rs` is its only unsafe surface on unix (IR-22,
//! IR-23). Every `unsafe` block below carries its own `// SAFETY:`.
//!
//! # Why a job object, and why it is better than what it replaces
//!
//! `tokio_runner`'s unix arm spawns each sheep with `process_group(0)`, so
//! the sheep leads a group of its own and a negative-pid `SIGKILL` reaches
//! the whole tree. Windows has no process groups in that sense. It has job
//! objects: a kernel container a process is assigned to, which its children
//! inherit automatically, and which can be terminated as a unit.
//!
//! The substitution is close to exact, and in one respect stronger.
//! `kill.rs`'s own module comment records a hole in the unix design — a
//! grandchild that calls `setsid` escapes its group and outlives
//! `kill_tree`. A job has no equivalent hole: a process cannot leave a job
//! it has been assigned to, and a child can only be spawned outside it if
//! the job itself was created with `JOB_OBJECT_LIMIT_BREAKAWAY_OK`, which
//! `Job::create` deliberately does not set. So `kill_tree` on Windows
//! reaches strictly more of the tree than its unix twin does.
//!
//! # What is deliberately NOT set
//!
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` would terminate every member the
//! moment the last handle to the job closed — which, since the daemon holds
//! that handle, means the whole flock would die with the shepherd.
//!
//! **That is not shep's behaviour on unix and it is not adopted here.** A
//! sheep whose shepherd is `SIGKILL`ed keeps running, orphaned; the flag
//! would make Windows the one platform where restarting the daemon also
//! stops every app it was supervising. Aligning the two platforms is worth
//! more than the flag's one genuine benefit (no orphan can outlive the
//! daemon and be duplicated by the next one's muster restore), and that
//! hazard is one shep already has, already documents, and should fix the
//! same way on both platforms if it fixes it at all.

#![allow(unsafe_code)] // IR-24 exception — the Windows twin of `sys.rs`'s, and
// the only one on this platform. Ten sites, all in this file: eight FFI calls
// (`CreateJobObjectW`, `SetInformationJobObject`, `AssignProcessToJobObject`,
// `TerminateJobObject`, `CloseHandle`, `GetStdHandle`, `SetHandleInformation`,
// and one `mem::zeroed` for a Win32 limit struct) plus the two `unsafe impl`s
// that make a kernel handle `Send` and `Sync`. Nothing outside this module
// writes `unsafe` on Windows.

use std::io;
use std::os::windows::io::RawHandle;

use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
};

/// Marks this process's own stdio handles non-inheritable.
///
/// **The Windows counterpart to `shep-cli`'s `seal_inherited_fds`, and it is
/// needed for the same reason that one is.** `launch.rs`'s Windows arm used
/// to carry a comment claiming no counterpart was necessary, on the grounds
/// that Windows inherits only handles explicitly marked inheritable. That is
/// true and is not the whole story: `CreateProcess` with `bInheritHandles =
/// TRUE` — which `std` passes whenever any stdio is redirected — inherits
/// **every** inheritable handle the parent holds, not only the ones `std`
/// prepared.
///
/// So a shepherd spawned from a shell that gave `shep` a PIPE for stdout
/// inherits that pipe, and holds it open for its entire life. The daemon
/// never writes to it (its own stdout is redirected to a log file), but the
/// pipe cannot close while a handle to it exists, so the shell blocks
/// forever waiting for output that will never end. Measured: `shep start |
/// Out-Null` hung indefinitely while `shep start` alone returned at once,
/// and `shep flock | Out-Null` — which spawns nothing — was unaffected.
///
/// That is the whole bug, and it is worse than it sounds because piping is
/// the normal case in the place it matters most: a CI job, a
/// `$(shep start ...)`, anything reading shep's output.
///
/// Called immediately before the spawn rather than once at startup: it
/// changes only the INHERIT flag, not the handles themselves, so this
/// process goes on using its own stdio exactly as before — the same
/// narrowness `seal_inherited_fds` has on unix, where it sets `FD_CLOEXEC`
/// and leaves the descriptors open.
///
/// Best-effort by design. A failure here means one spawn may inherit a
/// handle it should not, which is the behaviour that existed before this
/// function; refusing to start a shepherd over it would be the worse
/// outcome, so there is nothing to report and nothing for a caller to do.
pub fn seal_std_handles() {
    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: `GetStdHandle` takes one of the three documented standard
        // handle ids and returns a borrowed handle this process already
        // owns — it is not a new reference and must not be closed. It
        // returns `INVALID_HANDLE_VALUE` or null for a stream this process
        // does not have, both of which are filtered before use.
        let handle = unsafe { GetStdHandle(id) };
        if handle.is_null() || handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            continue;
        }
        // SAFETY: `handle` is a live handle owned by this process (checked
        // valid just above). Clearing `HANDLE_FLAG_INHERIT` changes only
        // whether a future `CreateProcess` passes it on; it neither closes
        // the handle nor affects any I/O this process performs through it.
        // The return value is ignored deliberately — see this function's
        // own doc for why a failure is not worth reporting.
        unsafe {
            let _ = SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
        }
    }
}

/// An anonymous job object owning one sheep and everything it spawns.
///
/// Closed on drop. Dropping does **not** terminate the members — see this
/// module's doc for why `KILL_ON_JOB_CLOSE` is not set.
#[derive(Debug)]
pub(crate) struct Job(HANDLE);

// SAFETY: a job object HANDLE is a kernel handle, not a thread-affine
// resource. Win32 permits any thread to call `AssignProcessToJobObject`,
// `TerminateJobObject` or `CloseHandle` on it, and the handle value is
// plain data. The daemon moves a `Job` into the per-sheep task that owns
// its `RunningProcess`, which is `Send`, so this impl is required and is
// sound for the same reason `std`'s own process handles are `Send`.
unsafe impl Send for Job {}
// SAFETY: as above; every Win32 call this module makes on the handle is
// itself thread-safe, and `Job` exposes no interior mutability.
unsafe impl Sync for Job {}

impl Job {
    /// Creates an anonymous job object.
    ///
    /// Anonymous (a null name) on purpose: a *named* job would be visible in
    /// the kernel object namespace, where another process could open it by
    /// name and terminate the sheep. Nothing needs to find this job except
    /// the daemon that created it, and the daemon holds the handle.
    ///
    /// # Errors
    ///
    /// Whatever the OS says if the job cannot be created or its limits set.
    pub(crate) fn create() -> io::Result<Self> {
        // SAFETY: both arguments are the documented "no security attributes,
        // no name" nulls. `CreateJobObjectW` returns a null handle on
        // failure, which is checked immediately below before the value is
        // stored or used.
        let handle = unsafe { CreateJobObjectW(core::ptr::null(), core::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Self(handle);

        // Zeroed, then left zeroed: every limit flag stays off. This call is
        // not strictly required today, since a job with no limits is exactly
        // what a default-constructed one already is — it is made anyway so
        // that the limit block is a real, named thing in this file rather
        // than an implicit default, and so that adding a limit later is a
        // one-line edit at a site that already handles its own errors.
        //
        // `JOB_OBJECT_LIMIT_BREAKAWAY_OK` and
        // `JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK` are the two flags whose
        // ABSENCE is load-bearing: without them a member cannot escape the
        // job, which is what makes `kill_tree` complete. See the module doc.
        // SAFETY: `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` is a plain Win32
        // POD struct — nested `LARGE_INTEGER`/`IO_COUNTERS`/`SIZE_T` fields
        // and nothing else. It holds no reference, no `NonNull` and no enum
        // with a restricted range, so all-zeroes is a valid inhabitant, and
        // is additionally the exact value wanted here: every limit flag off.
        let info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { core::mem::zeroed() };
        // SAFETY: `job.0` is a live job handle (checked non-null above).
        // `JobObjectExtendedLimitInformation` is the information class that
        // pairs with `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`, and the pointer
        // and length describe exactly that stack local, which outlives the
        // call. On failure the `Job` is dropped by the `?`, closing the
        // handle.
        let ok = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                core::ptr::from_ref(&info).cast(),
                u32::try_from(core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                    .expect("the size of a fixed Win32 struct fits in u32"),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    /// Assigns an already-spawned process to this job.
    ///
    /// Every process the assigned one goes on to create is a member too,
    /// automatically and unavoidably — that inheritance is what makes this
    /// the process-group substitute.
    ///
    /// # Race window
    ///
    /// There is a real one, and it is worth naming rather than implying it
    /// away: the child is spawned first and assigned immediately after, so a
    /// child that forks a grandchild in the microseconds before the
    /// assignment lands leaves that grandchild outside the job. Closing it
    /// entirely needs the child created suspended (`CREATE_SUSPENDED`) and
    /// resumed after assignment, which `std`'s `Command` cannot express and
    /// which would mean re-implementing `CreateProcessW` here. The window is
    /// the same shape as the unix arm's — `process_group(0)` is applied in
    /// the child between `fork` and `exec`, so it is narrower there — and it
    /// is not one an app hits by accident.
    ///
    /// # Errors
    ///
    /// Whatever the OS says. `ERROR_ACCESS_DENIED` here typically means the
    /// process is already in a job that does not permit breakaway, which on
    /// a developer machine most often means shep itself is running under one
    /// (some CI runners and terminal hosts do this).
    pub(crate) fn assign(&self, process: RawHandle) -> io::Result<()> {
        // SAFETY: `self.0` is a live job handle for as long as `self` lives.
        // `process` is the caller's live child-process handle, borrowed from
        // a `tokio::process::Child` that outlives this call — the runner
        // assigns immediately after spawn and holds the `Child` afterwards.
        // Neither handle is consumed by this call.
        let ok = unsafe { AssignProcessToJobObject(self.0, process as HANDLE) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Terminates every process in the job.
    ///
    /// The `kill_tree` rung: unblockable, exactly like the `SIGKILL` it
    /// stands in for. `exit_code` becomes the observed exit code of every
    /// member.
    ///
    /// # Errors
    ///
    /// Whatever the OS says. A job whose members have all already exited is
    /// **not** an error — this succeeds and does nothing, which is the same
    /// shape as `SIGKILL`ing an already-dead group.
    pub(crate) fn terminate(&self, exit_code: u32) -> io::Result<()> {
        // SAFETY: `self.0` is a live job handle for as long as `self` lives.
        let ok = unsafe { TerminateJobObject(self.0, exit_code) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: `self.0` was returned non-null by `CreateJobObjectW` and
        // has not been closed — nothing else in this module closes it, and
        // `Job` is neither `Copy` nor `Clone`, so this runs exactly once.
        // The return value is ignored deliberately: a failing `CloseHandle`
        // in a destructor has no caller to report to, and the process is
        // either exiting or leaking one handle.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fails if a job cannot be created and terminated on this machine.
    ///
    /// Terminating an EMPTY job is the case worth pinning: `kill_tree` runs
    /// unconditionally at the top of the ladder's second rung, including
    /// against a sheep that already exited, so "no members" must read as
    /// success rather than as an error the ladder would log.
    #[test]
    fn an_empty_job_is_created_and_terminated_without_error() {
        let job = Job::create().expect("a job object must be creatable");
        job.terminate(1)
            .expect("terminating a job with no members must succeed");
    }

    /// fails if a real child assigned to a job survives `terminate`.
    ///
    /// The load-bearing assertion of this module: this is `kill_tree`. Uses
    /// a child that would otherwise run far longer than the test, so an exit
    /// can only be the termination.
    #[tokio::test]
    async fn a_child_assigned_to_a_job_is_killed_by_terminate() {
        let job = Job::create().unwrap();
        // `ping`, not `timeout /t`: `timeout.exe` refuses a non-console stdin
        // and exits in milliseconds, so it would assert nothing here. Same
        // choice, same measurement, as `real_runner_windows.rs`'s
        // `LONG_RUNNING`.
        let mut child = tokio::process::Command::new("cmd")
            .args(["/C", "ping", "-n", "600", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .stdin(std::process::Stdio::null())
            .spawn()
            .expect("cmd must be spawnable on Windows");

        let handle = child
            .raw_handle()
            .expect("a just-spawned child has a raw handle");
        job.assign(handle).expect("assignment must succeed");

        job.terminate(9).unwrap();

        let status = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait())
            .await
            .expect("a terminated job member must not outlive its job")
            .unwrap();
        assert!(
            !status.success(),
            "a job-terminated child must not report success"
        );
    }
}
